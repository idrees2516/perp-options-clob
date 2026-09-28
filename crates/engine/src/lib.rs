//! # poc-engine
//!
//! The deterministic sequencer: **commands in, events out, state applied**.
//!
//! ## The architecture in one paragraph
//!
//! [`Engine::process`] turns a [`Command`] into a batch of [`Event`]s and
//! applies them; every state mutation in the system flows through
//! [`Engine::apply_event`], the single mutator. Replaying the same event
//! journal into a fresh engine reproduces the state bit-for-bit — the
//! dYdX v4 / Derive V3 determinism model, and the property that makes
//! off-chain matching with on-chain settlement auditable.
//!
//! ```text
//!      Command ──▶ plan(&self)  ──▶ Vec<Event>  ──▶ apply(&mut self) ──▶ state
//!        │                          │                                     ▲
//!        │                          └────────── the journal ───────────────┘
//!      Tick    ──▶ sweep: funding · expiry · GTD · rewards · halts · liquidations
//! ```
//!
//! ## Mark policy (why margining never touches the order book)
//!
//! Marks are **oracle-anchored, never book-derived**: the perp mark is the
//! oracle spot (dYdX v4 margins on oracle), the option mark is
//! Black-Scholes at the oracle spot with a configured IV. A mark fed by
//! the book would let a single large order move every account's margin —
//! circularity and a manipulation vector. The book is only used for the
//! *funding premium* (BBO-mid TWAP vs index TWAP) where clamps bound its
//! influence.
//!
//! ## Fail-safe behaviour
//!
//! * Oracle quorum lost → the underlying **halts** (no new orders); marks
//!   are stale so liquidations are *skipped* — never liquidate on a price
//!   nobody can defend.
//! * Missing marks for a held position → margining fails safe (orders
//!   rejected, risk systems refuse to guess).
//! * Every arithmetic path is checked/saturating; the engine cannot panic
//!   on degraded input.

pub mod algorithms;
pub mod amend;
pub mod auction;
pub mod collateral;
pub mod command;
pub mod engine;
pub mod event;
pub mod institutions;
pub mod listing;
pub mod mm;
pub mod sweep;
pub mod vaults;

pub use collateral::{CollateralCurrency, PriceSource, QUOTE_CODE};
pub use command::{Command, OrderRequest, RfqLegCommand};
pub use engine::{Engine, EngineConfig, EngineStats, ListingPolicy, PorLiabilityRow};
pub use event::{
    AccountView, AdlExecuted, BookView, Event, FundingPaid, FundingSettled, LiquidationExecuted,
    LiquidityObservation, MarketStateView, OptionSettled, OrderCloseReason, OrderRejected,
    RewardPaid, Trade, TwapParent, VaultEpoch,
};
pub use listing::{default_btc_template, OptionTemplate, UnderlyingListing};

#[cfg(test)]
mod greeks_limit_tests {
    use crate::{Command, Engine, EngineConfig, Event, OrderRequest};
    use poc_core::{Instrument, OptionMarket, Side, TimestampMs};
    use poc_risk::{GreeksLimits, Rejection};

    const T0: TimestampMs = 1_000_000;

    /// G-41: an option order that would push the portfolio vega over
    /// the configured cap is rejected pre-trade.
    #[test]
    fn vega_cap_rejects_option_order() {
        let cfg = EngineConfig {
            greeks_limits: GreeksLimits {
                max_abs_vega_quote_minor_per_pct: 1,
                max_abs_gamma_quote_minor_per_pct: 0,
            },
            ..EngineConfig::default()
        };
        // ATM 30d BTC call at 55% IV: vega per lot is ~
        let mut e = Engine::new(cfg);
        let opt = OptionMarket {
            expiry_ts_ms: T0 + 30 * 86_400_000,
            ..OptionMarket::default()
        };
        e.register_instrument(Instrument::Option(opt));
        for provider in ["pyth", "chainlink"] {
            e.process(Command::OracleUpdate {
                base_symbol: "BTC".into(),
                provider: provider.into(),
                ts: T0,
                price_quote_minor: 8_000_000,
            });
        }
        e.process(Command::Tick { now: T0 });
        e.process(Command::Deposit {
            subaccount: 1,
            amount_quote_minor: 100_000_000,
        });

        let ev = e.process(Command::Place {
            request: OrderRequest::limit(1, "BTC-19800-80000-C", Side::Bid, 10_000, 1),
            now: T0 + 1,
        });
        assert!(
            ev.iter().any(|x| matches!(
                x,
                Event::OrderRejection(r) if matches!(
                    r.reason,
                    Rejection::GreeksLimitExceeded { what: "vega", .. }
                )
            )),
            "expected a vega-cap rejection, got {ev:?}"
        );

        // Raising the cap to effectively-unbounded lets the order rest.
        let cfg2 = EngineConfig {
            greeks_limits: GreeksLimits {
                max_abs_vega_quote_minor_per_pct: 1_000_000_000,
                max_abs_gamma_quote_minor_per_pct: 0,
            },
            ..EngineConfig::default()
        };
        let mut e2 = Engine::new(cfg2);
        e2.register_instrument(Instrument::Option(OptionMarket {
            expiry_ts_ms: T0 + 30 * 86_400_000,
            ..OptionMarket::default()
        }));
        for provider in ["pyth", "chainlink"] {
            e2.process(Command::OracleUpdate {
                base_symbol: "BTC".into(),
                provider: provider.into(),
                ts: T0,
                price_quote_minor: 8_000_000,
            });
        }
        e2.process(Command::Tick { now: T0 });
        e2.process(Command::Deposit {
            subaccount: 1,
            amount_quote_minor: 100_000_000,
        });
        let ev2 = e2.process(Command::Place {
            request: OrderRequest::limit(1, "BTC-19800-80000-C", Side::Bid, 10_000, 1),
            now: T0 + 1,
        });
        assert!(!ev2.iter().any(|x| matches!(
            x,
            Event::OrderRejection(r) if matches!(r.reason, Rejection::GreeksLimitExceeded { .. })
        )));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Event, OrderCloseReason};
    use poc_core::{Instrument, OptionMarket, PerpMarket, Side, TimestampMs};

    const T0: TimestampMs = 1_000_000;

    fn config() -> EngineConfig {
        EngineConfig::default()
    }

    fn engine_with_market() -> Engine {
        let mut e = Engine::new(config());
        e.register_instrument(Instrument::Perp(PerpMarket::default()));
        e
    }

    /// Seed two oracle providers to quorum at `price` and tick.
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

    #[test]
    fn deposits_create_accounts() {
        let mut e = Engine::new(config());
        e.process(Command::Deposit {
            subaccount: 1,
            amount_quote_minor: 1_000,
        });
        assert_eq!(e.account(1).map(|a| a.cash_quote_minor), Some(1_000));
        e.process(Command::Deposit {
            subaccount: 1,
            amount_quote_minor: 500,
        });
        assert_eq!(e.account(1).map(|a| a.cash_quote_minor), Some(1_500));
        assert!(e.account(2).is_none());
    }

    #[test]
    fn withdraw_gated_by_free_equity() {
        let mut e = engine_with_market();
        seed_oracle(&mut e, T0, 8_000_000);
        e.process(Command::Deposit {
            subaccount: 1,
            amount_quote_minor: 1_000_000,
        });
        // Open a position that consumes margin.
        e.process(Command::Place {
            request: OrderRequest::limit(1, "BTC-PERP", Side::Bid, 80_000, 1000),
            now: T0,
        });
        // Free equity < 1M now: withdrawal rejected.
        let events = e.process(Command::Withdraw {
            subaccount: 1,
            amount_quote_minor: 1_000_000,
        });
        assert!(matches!(events[0], Event::WithdrawRejected { .. }));
        // A small withdrawal still works (equity >> initial).
        let events = e.process(Command::Withdraw {
            subaccount: 1,
            amount_quote_minor: 100,
        });
        assert!(matches!(events[0], Event::Withdrawal { .. }));
    }

    #[test]
    fn limit_order_matches_and_books() {
        let mut e = engine_with_market();
        seed_oracle(&mut e, T0, 8_000_000);
        e.process(Command::Deposit {
            subaccount: 1,
            amount_quote_minor: 10_000_000,
        });
        e.process(Command::Deposit {
            subaccount: 2,
            amount_quote_minor: 10_000_000,
        });

        // Maker sells at 80_100.
        let ev = e.process(Command::Place {
            request: OrderRequest::limit(2, "BTC-PERP", Side::Ask, 80_100, 50),
            now: T0,
        });
        assert!(matches!(ev[0], Event::OrderResting { .. }));

        // Taker buys at 80_100: crosses, partially filling the maker.
        let ev = e.process(Command::Place {
            request: OrderRequest::limit(1, "BTC-PERP", Side::Bid, 80_100, 20),
            now: T0,
        });
        assert_eq!(
            ev.len(),
            2,
            "trade + taker closed (maker still rests): {ev:?}"
        );
        assert!(matches!(ev[0], Event::TradeExecuted(_)));
        assert!(matches!(
            ev[1],
            Event::OrderClosed {
                reason: OrderCloseReason::Filled,
                ..
            }
        ));

        assert_eq!(e.account(1).map(|a| a.lots_of("BTC-PERP")), Some(20));
        assert_eq!(e.account(2).map(|a| a.lots_of("BTC-PERP")), Some(-20));
        assert_eq!(e.book("BTC-PERP").map(|b| b.open_order_count()), Some(1));
        assert_eq!(e.account(2).map(|a| a.open_orders.len()), Some(1));
    }

    #[test]
    fn fees_flow_and_route() {
        let mut e = engine_with_market();
        seed_oracle(&mut e, T0, 8_000_000);
        e.process(Command::Deposit {
            subaccount: 1,
            amount_quote_minor: 10_000_000,
        });
        e.process(Command::Deposit {
            subaccount: 2,
            amount_quote_minor: 10_000_000,
        });
        e.process(Command::Place {
            request: OrderRequest::limit(2, "BTC-PERP", Side::Ask, 80_000, 100),
            now: T0,
        });
        e.process(Command::Place {
            request: OrderRequest::limit(1, "BTC-PERP", Side::Bid, 80_000, 100),
            now: T0,
        });
        // Notional = 100 lots * $80 = $8,000 = 800_000 minor.
        // Base tier: taker 4 bps = 320, maker 1 bp = 80.
        let fees_taker = e.account(1).map(|a| a.fees_paid_quote_minor);
        let fees_maker = e.account(2).map(|a| a.fees_paid_quote_minor);
        assert_eq!(fees_taker, Some(320));
        assert_eq!(fees_maker, Some(80));
        // Routed 60/30/10 of gross 400: house 240, insurance 120, buyback 40.
        let stats = e.stats();
        assert_eq!(stats.revenue.house, 240);
        assert_eq!(stats.revenue.insurance, 120);
        assert_eq!(stats.revenue.buyback, 40);
    }

    #[test]
    fn post_only_rejects_crossing() {
        let mut e = engine_with_market();
        seed_oracle(&mut e, T0, 8_000_000);
        e.process(Command::Deposit {
            subaccount: 1,
            amount_quote_minor: 10_000_000,
        });
        e.process(Command::Deposit {
            subaccount: 2,
            amount_quote_minor: 10_000_000,
        });
        e.process(Command::Place {
            request: OrderRequest::limit(2, "BTC-PERP", Side::Ask, 80_100, 50),
            now: T0,
        });
        let mut req = OrderRequest::limit(1, "BTC-PERP", Side::Bid, 80_100, 20);
        req.post_only = true;
        let ev = e.process(Command::Place {
            request: req,
            now: T0,
        });
        assert!(matches!(ev[0], Event::OrderRejection(_)));
    }

    #[test]
    fn margin_gate_rejects_orders() {
        let mut e = engine_with_market();
        seed_oracle(&mut e, T0, 8_000_000);
        e.process(Command::Deposit {
            subaccount: 1,
            amount_quote_minor: 500,
        });
        let ev = e.process(Command::Place {
            request: OrderRequest::limit(1, "BTC-PERP", Side::Bid, 80_000, 10_000),
            now: T0,
        });
        match &ev[0] {
            Event::OrderRejection(r) => {
                assert!(matches!(
                    r.reason,
                    poc_risk::Rejection::InsufficientMargin { .. }
                ));
            }
            other => panic!("expected rejection, got {other:?}"),
        }
    }

    #[test]
    fn oracle_quorum_loss_halts_and_resumes() {
        let mut e = engine_with_market();
        let t = T0;
        for provider in ["pyth", "chainlink"] {
            e.process(Command::OracleUpdate {
                base_symbol: "BTC".into(),
                provider: provider.into(),
                ts: t,
                price_quote_minor: 8_000_000,
            });
        }
        e.process(Command::Tick { now: t });
        // Both providers go silent: quorum lost at t + staleness.
        let stale = t + 61_000;
        let ev = e.process(Command::Tick { now: stale });
        assert!(ev.iter().any(|x| matches!(x, Event::MarketHalted { .. })));
        assert_eq!(e.book_view("BTC-PERP").map(|b| b.halted), Some(true));

        // Orders are rejected while halted.
        e.process(Command::Deposit {
            subaccount: 1,
            amount_quote_minor: 10_000_000,
        });
        let ev = e.process(Command::Place {
            request: OrderRequest::limit(1, "BTC-PERP", Side::Bid, 80_000, 10),
            now: stale,
        });
        assert!(matches!(ev[0], Event::OrderRejection(_)));

        // Providers come back: quorum restored.
        for provider in ["pyth", "chainlink"] {
            e.process(Command::OracleUpdate {
                base_symbol: "BTC".into(),
                provider: provider.into(),
                ts: stale + 1_000,
                price_quote_minor: 8_000_000,
            });
        }
        let ev = e.process(Command::Tick { now: stale + 1_000 });
        assert!(ev.iter().any(|x| matches!(x, Event::MarketResumed { .. })));
        assert_eq!(e.book_view("BTC-PERP").map(|b| b.halted), Some(false));
    }

    #[test]
    fn rogue_provider_is_quarantined() {
        let mut e = engine_with_market();
        seed_oracle(&mut e, T0, 8_000_000);
        // A rogue print far off the mark: quarantined, mark unaffected.
        e.process(Command::OracleUpdate {
            base_symbol: "BTC".into(),
            provider: "rogue".into(),
            ts: T0 + 1,
            price_quote_minor: 40_000_000,
        });
        let spot = e
            .market_state()
            .spots
            .iter()
            .find(|(b, _)| b == "BTC")
            .and_then(|(_, s)| *s);
        assert_eq!(spot, Some(8_000_000));
    }

    #[test]
    fn funding_settles_on_interval() {
        let mut e = engine_with_market();
        let interval = 8 * 3_600_000;
        let mut t = T0;
        // Establish quorum and a rich market (book mid above index).
        for provider in ["pyth", "chainlink"] {
            e.process(Command::OracleUpdate {
                base_symbol: "BTC".into(),
                provider: provider.into(),
                ts: t,
                price_quote_minor: 8_000_000,
            });
        }
        e.process(Command::Deposit {
            subaccount: 1,
            amount_quote_minor: 100_000_000,
        });
        e.process(Command::Deposit {
            subaccount: 2,
            amount_quote_minor: 100_000_000,
        });
        // Long vs short via market orders crossing on the book.
        e.process(Command::Place {
            request: OrderRequest::limit(2, "BTC-PERP", Side::Ask, 80_200, 1000),
            now: t,
        });
        e.process(Command::Place {
            request: OrderRequest::limit(1, "BTC-PERP", Side::Bid, 80_200, 1000),
            now: t,
        });
        assert_eq!(e.account(1).map(|a| a.lots_of("BTC-PERP")), Some(1000));
        assert_eq!(e.account(2).map(|a| a.lots_of("BTC-PERP")), Some(-1000));

        // Advance past the funding interval; the mark TWAP (last trade at
        // 80_200 vs index 80_000) makes longs pay.
        t += interval + 1;
        e.process(Command::OracleUpdate {
            base_symbol: "BTC".into(),
            provider: "pyth".into(),
            ts: t,
            price_quote_minor: 8_000_000,
        });
        e.process(Command::OracleUpdate {
            base_symbol: "BTC".into(),
            provider: "chainlink".into(),
            ts: t,
            price_quote_minor: 8_000_000,
        });
        let ev = e.process(Command::Tick { now: t });
        let funding_events: Vec<&Event> = ev
            .iter()
            .filter(|x| matches!(x, Event::Funding(_)))
            .collect();
        assert_eq!(funding_events.len(), 1, "one interval settled: {ev:?}");
        if let Event::Funding(f) = funding_events[0] {
            assert!(f.rate_bps > 0, "rich mark -> longs pay: {}", f.rate_bps);
        }
        // The long paid, the short received (conservation).
        let long_funding = e.account(1).map(|a| a.funding_pnl_quote_minor);
        let short_funding = e.account(2).map(|a| a.funding_pnl_quote_minor);
        assert!(long_funding.is_some_and(|x| x > 0), "positive = paid");
        assert!(short_funding.is_some_and(|x| x < 0), "negative = received");
        assert_eq!(long_funding, short_funding.map(|x| -x), "funding conserves");
    }

    #[test]
    fn option_settles_at_expiry_on_twap() {
        let mut e = Engine::new(config());
        // 80k call, 0.01 lots, $0.50 ticks, expiring 30 days out.
        let opt = OptionMarket {
            expiry_ts_ms: T0 + 30 * 86_400_000,
            ..OptionMarket::default()
        };
        e.register_instrument(Instrument::Option(opt.clone()));
        seed_oracle(&mut e, T0, 8_000_000);
        e.process(Command::Deposit {
            subaccount: 1,
            amount_quote_minor: 100_000_000,
        });
        e.process(Command::Deposit {
            subaccount: 2,
            amount_quote_minor: 100_000_000,
        });

        // ATM call, 30d, iv .55 → mark ≈ $5k per BTC ≈ 10_000 ticks.
        // Trade the option at a 10_000-tick premium (≈ $5k per BTC).
        let premium_ticks = 10_000;
        e.process(Command::Place {
            request: OrderRequest::limit(2, "BTC-19800-80000-C", Side::Ask, premium_ticks, 100),
            now: T0,
        });
        let ev = e.process(Command::Place {
            request: OrderRequest::limit(1, "BTC-19800-80000-C", Side::Bid, premium_ticks, 100),
            now: T0,
        });
        assert!(
            ev.iter().any(|x| matches!(x, Event::TradeExecuted(_))),
            "{ev:?}"
        );
        assert_eq!(
            e.account(1).map(|a| a.lots_of("BTC-19800-80000-C")),
            Some(100)
        );

        // Move spot ITM (90k) and expire: ramp the price up in <5% steps
        // (a single 12.5% print would be quarantined), then hold 9M across
        // the whole 30-minute settlement window so the TWAP = 9M.
        let expiry = T0 + 30 * 86_400_000;
        for (i, &p) in [8_320_000_u128, 8_650_000, 9_000_000].iter().enumerate() {
            let ts = expiry - 45 * 60_000 + (i as TimestampMs) * 5 * 60_000;
            for provider in ["pyth", "chainlink"] {
                e.process(Command::OracleUpdate {
                    base_symbol: "BTC".into(),
                    provider: provider.into(),
                    ts,
                    price_quote_minor: p,
                });
            }
        }
        for step in 0..7 {
            let ts = expiry - (6 - step) * 5 * 60_000;
            for provider in ["pyth", "chainlink"] {
                e.process(Command::OracleUpdate {
                    base_symbol: "BTC".into(),
                    provider: provider.into(),
                    ts,
                    price_quote_minor: 9_000_000,
                });
            }
        }
        let ev = e.process(Command::Tick { now: expiry });
        let settles: Vec<&Event> = ev
            .iter()
            .filter(|x| matches!(x, Event::OptionExpiry(_)))
            .collect();
        assert_eq!(settles.len(), 2, "long and short settle: {ev:?}");
        // Long receives intrinsic: (90k − 80k) × 1.0 BTC = $10k.
        for s in settles {
            if let Event::OptionExpiry(s) = s {
                assert_eq!(s.settlement_quote_minor, 9_000_000);
                if s.subaccount == 1 {
                    assert_eq!(s.payout_quote_minor, 1_000_000);
                } else {
                    assert_eq!(s.payout_quote_minor, -1_000_000);
                }
            }
        }
        // Positions gone, instrument delisted.
        assert_eq!(
            e.account(1).map(|a| a.lots_of("BTC-19800-80000-C")),
            Some(0)
        );
        assert!(e.instruments().get("BTC-19800-80000-C").is_none());
    }

    #[test]
    fn liquidation_closes_underwater_position() {
        let mut e = engine_with_market();
        seed_oracle(&mut e, T0, 8_000_000);
        // Just enough to open 1.0 BTC (initial ≈ $4.2k) — nothing more.
        e.process(Command::Deposit {
            subaccount: 1,
            amount_quote_minor: 450_000,
        });
        e.process(Command::Deposit {
            subaccount: 2,
            amount_quote_minor: 100_000_000,
        });

        // Account 1 goes long 1.0 BTC; account 2 takes the other side.
        e.process(Command::Place {
            request: OrderRequest::limit(2, "BTC-PERP", Side::Ask, 80_000, 1000),
            now: T0,
        });
        e.process(Command::Place {
            request: OrderRequest::limit(1, "BTC-PERP", Side::Bid, 80_000, 1000),
            now: T0,
        });
        assert_eq!(e.account(1).map(|a| a.lots_of("BTC-PERP")), Some(1000));

        // Walk the spot down in oracle-sized steps (a single 10% print
        // would be quarantined as a deviation): equity goes under
        // maintenance around −5%.
        let mut t = T0;
        let mut price = 8_000_000_u128;
        let mut liquidated = false;
        for _ in 0..8 {
            t += 30_000;
            price = price.saturating_sub(80_000); // 1% steps
            for provider in ["pyth", "chainlink"] {
                e.process(Command::OracleUpdate {
                    base_symbol: "BTC".into(),
                    provider: provider.into(),
                    ts: t,
                    price_quote_minor: price,
                });
            }
            let ev = e.process(Command::Tick { now: t });
            if ev.iter().any(|x| matches!(x, Event::Liquidation(_))) {
                liquidated = true;
                break;
            }
        }
        assert!(liquidated, "liquidation must fire within a 8% decline");
        // The account shrank or closed.
        let lots = e.account(1).map(|a| a.lots_of("BTC-PERP"));
        assert!(lots.unwrap_or(0) < 1000, "position must shrink");
    }

    #[test]
    fn reduce_only_never_increases_position() {
        let mut e = engine_with_market();
        seed_oracle(&mut e, T0, 8_000_000);
        e.process(Command::Deposit {
            subaccount: 1,
            amount_quote_minor: 10_000_000,
        });
        e.process(Command::Deposit {
            subaccount: 2,
            amount_quote_minor: 10_000_000,
        });
        e.process(Command::Place {
            request: OrderRequest::limit(2, "BTC-PERP", Side::Ask, 80_000, 100),
            now: T0,
        });
        e.process(Command::Place {
            request: OrderRequest::limit(1, "BTC-PERP", Side::Bid, 80_000, 100),
            now: T0,
        });
        assert_eq!(e.account(1).map(|a| a.lots_of("BTC-PERP")), Some(100));
        // A reduce-only SELL of 300 caps at the current 100-lot position.
        let mut req = OrderRequest::limit(1, "BTC-PERP", Side::Ask, 80_000, 300);
        req.reduce_only = true;
        let ev = e.process(Command::Place {
            request: req,
            now: T0,
        });
        // Capped order fully filled against maker 2? maker is out of
        // liquidity — order rests reduced at 100 lots.
        let resting = ev.iter().find_map(|x| match x {
            Event::OrderResting { order, .. } => Some(order.qty_lots),
            _ => None,
        });
        assert_eq!(resting, Some(100), "capped to position size");
    }

    #[test]
    fn stop_market_triggers_on_mark_cross() {
        let mut e = engine_with_market();
        seed_oracle(&mut e, T0, 8_000_000);
        e.process(Command::Deposit {
            subaccount: 1,
            amount_quote_minor: 100_000_000,
        });
        e.process(Command::Deposit {
            subaccount: 2,
            amount_quote_minor: 100_000_000,
        });

        // Maker provides a deep bid at 75_000.
        e.process(Command::Place {
            request: OrderRequest::limit(2, "BTC-PERP", Side::Bid, 75_000, 500),
            now: T0,
        });
        // Stop-sell triggers below 79_000.
        let mut req = OrderRequest::limit(1, "BTC-PERP", Side::Ask, 0, 100);
        req.order_type = poc_core::OrderType::StopMarket {
            trigger_price: 79_000,
        };
        req.price_ticks = None;
        let ev = e.process(Command::Place {
            request: req,
            now: T0,
        });
        assert!(
            matches!(ev[0], Event::OrderResting { .. }),
            "stop parks: {ev:?}"
        );

        // Mark crashes through the trigger.
        let t = T0 + 60_000;
        for provider in ["pyth", "chainlink"] {
            e.process(Command::OracleUpdate {
                base_symbol: "BTC".into(),
                provider: provider.into(),
                ts: t,
                price_quote_minor: 7_800_000,
            });
        }
        let ev = e.process(Command::Tick { now: t });
        assert!(
            ev.iter().any(|x| matches!(x, Event::TradeExecuted(_))),
            "stop must fire: {ev:?}"
        );
        assert_eq!(e.account(1).map(|a| a.lots_of("BTC-PERP")), Some(-100));
    }

    #[test]
    fn rewards_pay_makers_pro_rata() {
        let mut e = engine_with_market();
        let reward_interval = 3_600_000;
        e.config.reward_interval_ms = reward_interval; // fields are pub(crate); tests can touch
        e.next_reward_ts = T0 + reward_interval;
        seed_oracle(&mut e, T0, 8_000_000);
        e.process(Command::Deposit {
            subaccount: 1,
            amount_quote_minor: 100_000_000,
        });
        e.process(Command::Deposit {
            subaccount: 2,
            amount_quote_minor: 100_000_000,
        });

        // Two makers quote two-sided around the mark.
        e.process(Command::Place {
            request: OrderRequest::limit(1, "BTC-PERP", Side::Bid, 79_900, 10),
            now: T0,
        });
        e.process(Command::Place {
            request: OrderRequest::limit(1, "BTC-PERP", Side::Ask, 80_100, 10),
            now: T0,
        });
        e.process(Command::Place {
            request: OrderRequest::limit(2, "BTC-PERP", Side::Bid, 79_950, 5),
            now: T0,
        });
        e.process(Command::Place {
            request: OrderRequest::limit(2, "BTC-PERP", Side::Ask, 80_050, 5),
            now: T0,
        });

        // Keep the oracle fresh across the interval.
        for provider in ["pyth", "chainlink"] {
            e.process(Command::OracleUpdate {
                base_symbol: "BTC".into(),
                provider: provider.into(),
                ts: T0 + reward_interval,
                price_quote_minor: 8_000_000,
            });
        }
        let ev = e.process(Command::Tick {
            now: T0 + reward_interval,
        });
        let rewards: Vec<&Event> = ev
            .iter()
            .filter(|x| matches!(x, Event::Reward(_)))
            .collect();
        assert_eq!(rewards.len(), 2, "both makers rewarded: {ev:?}");
        assert!(
            ev.iter().any(|x| matches!(x, Event::RewardsSettled)),
            "interval closes"
        );
    }

    #[test]
    fn conservation_across_trades_and_funding() {
        let mut e = engine_with_market();
        let interval = 8 * 3_600_000;
        let mut t = T0;
        for provider in ["pyth", "chainlink"] {
            e.process(Command::OracleUpdate {
                base_symbol: "BTC".into(),
                provider: provider.into(),
                ts: t,
                price_quote_minor: 8_000_000,
            });
        }
        e.process(Command::Deposit {
            subaccount: 1,
            amount_quote_minor: 50_000_000,
        });
        e.process(Command::Deposit {
            subaccount: 2,
            amount_quote_minor: 50_000_000,
        });

        // A sequence of crossed trades.
        for round in 0..5 {
            let price = 80_000 + round * 10;
            e.process(Command::Place {
                request: OrderRequest::limit(2, "BTC-PERP", Side::Ask, price, 50),
                now: t,
            });
            e.process(Command::Place {
                request: OrderRequest::limit(1, "BTC-PERP", Side::Bid, price, 50),
                now: t,
            });
        }

        // Cash + positions = deposits − fees − revenue routed out.
        let stats = e.stats();
        let cash: i128 = [1, 2]
            .iter()
            .map(|&s| e.account(s).map(|a| a.cash_quote_minor).unwrap_or(0))
            .sum();
        let deposits: i128 = 100_000_000;
        let fees_routed = stats.revenue.total();
        assert_eq!(
            deposits - cash,
            fees_routed as i128,
            "cash left only via fees, which route exactly"
        );

        // Funding conserves.
        t += interval + 1;
        for provider in ["pyth", "chainlink"] {
            e.process(Command::OracleUpdate {
                base_symbol: "BTC".into(),
                provider: provider.into(),
                ts: t,
                price_quote_minor: 8_000_000,
            });
        }
        e.process(Command::Tick { now: t });
        let f1 = e.account(1).map(|a| a.funding_pnl_quote_minor).unwrap_or(0);
        let f2 = e.account(2).map(|a| a.funding_pnl_quote_minor).unwrap_or(0);
        assert_eq!(f1 + f2, 0, "funding is zero-sum");
    }

    // ------------------------------------------------------------------
    // New-feature integration tests (G-01/02/04, G-11/13, G-31, G-37, MMP)
    // ------------------------------------------------------------------

    fn wing_call() -> OptionMarket {
        OptionMarket {
            symbol: "BTC-EVER-80000-C".into(),
            variant: poc_core::OptionVariant::Everlasting,
            everlasting: poc_core::EverlastingParams {
                interval_ms: 60_000,
                maturity_multiple: 43_200,
            },
            price_band_bps: 100_000, // any price rests: fees are under test
            ..OptionMarket::default()
        }
    }

    fn everlasting_call() -> OptionMarket {
        OptionMarket {
            symbol: "BTC-EVER-80000-C".into(),
            variant: poc_core::OptionVariant::Everlasting,
            everlasting: poc_core::EverlastingParams {
                interval_ms: 60_000, // 1-minute rolls for a fast test
                maturity_multiple: 43_200,
            },
            expiry_ts_ms: 0,
            ..OptionMarket::default()
        }
    }

    #[test]
    fn everlasting_roll_settles_and_conserves() {
        let mut e = Engine::new(config());
        e.register_instrument(Instrument::Option(everlasting_call()));
        seed_oracle(&mut e, T0, 8_000_000);
        e.process(Command::Deposit {
            subaccount: 1,
            amount_quote_minor: 100_000_000,
        });
        e.process(Command::Deposit {
            subaccount: 2,
            amount_quote_minor: 100_000_000,
        });
        // Long 100 lots vs short 100 lots at a 10_000-tick premium.
        e.process(Command::Place {
            request: OrderRequest::limit(2, "BTC-EVER-80000-C", Side::Ask, 10_000, 100),
            now: T0,
        });
        e.process(Command::Place {
            request: OrderRequest::limit(1, "BTC-EVER-80000-C", Side::Bid, 10_000, 100),
            now: T0,
        });
        assert_eq!(
            e.account(1).map(|a| a.lots_of("BTC-EVER-80000-C")),
            Some(100)
        );
        // Advance past one roll interval with a live oracle.
        let t1 = T0 + 61_000;
        for provider in ["pyth", "chainlink"] {
            e.process(Command::OracleUpdate {
                base_symbol: "BTC".into(),
                provider: provider.into(),
                ts: t1,
                price_quote_minor: 8_000_000,
            });
        }
        let ev = e.process(Command::Tick { now: t1 });
        assert!(
            ev.iter().any(|x| matches!(x, Event::Funding(_))),
            "everlasting roll settles: {ev:?}"
        );
        // Longs paid, shorts received, conservation exact.
        let long = e.account(1).map(|a| a.funding_pnl_quote_minor);
        let short = e.account(2).map(|a| a.funding_pnl_quote_minor);
        assert!(long.is_some_and(|x| x > 0), "long pays the roll: {long:?}");
        assert_eq!(
            long,
            short.map(|x| -x),
            "roll conserves across the closed set"
        );
        // The everlasting position persists (never expires, never delists).
        assert_eq!(
            e.account(1).map(|a| a.lots_of("BTC-EVER-80000-C")),
            Some(100)
        );
        assert!(e.instruments().contains_key("BTC-EVER-80000-C"));
    }

    #[test]
    fn option_fee_cap_binds_on_wing_trades() {
        let mut e = Engine::new(config());
        e.register_instrument(Instrument::Option(wing_call()));
        seed_oracle(&mut e, T0, 8_000_000);
        e.process(Command::Deposit {
            subaccount: 1,
            amount_quote_minor: 100_000_000,
        });
        e.process(Command::Deposit {
            subaccount: 2,
            amount_quote_minor: 100_000_000,
        });
        // Deep wing: 1-tick premium ($0.50) x 1 lot (0.01 BTC) -> premium 5c.
        // Uncapped taker rate (4bps of ~$800 underlying notional = 3.2c-ish)
        // vs cap 12.5% of 5c = 0.625c -> cap binds at 1 minor unit.
        e.process(Command::Place {
            request: OrderRequest::limit(2, "BTC-EVER-80000-C", Side::Ask, 1, 1),
            now: T0,
        });
        let ev = e.process(Command::Place {
            request: OrderRequest::limit(1, "BTC-EVER-80000-C", Side::Bid, 1, 1),
            now: T0,
        });
        let trade = ev.iter().find_map(|x| match x {
            Event::TradeExecuted(t) => Some(t.as_ref().clone()),
            _ => None,
        });
        let trade = trade.expect("wing trade fills");
        let premium = trade.notional_quote_minor;
        let cap = premium / 8; // 12.5% of premium
        assert!(
            u128::try_from(trade.taker_fee_quote_minor).unwrap_or(0) <= cap.max(1),
            "taker fee {} must be capped near 12.5% of premium {premium}",
            trade.taker_fee_quote_minor
        );
    }

    #[test]
    fn rfq_lifecycle_quotes_and_executes() {
        use crate::command::RfqLegCommand;
        let mut e = engine_with_market();
        seed_oracle(&mut e, T0, 8_000_000);
        e.process(Command::Deposit {
            subaccount: 1,
            amount_quote_minor: 100_000_000,
        });
        e.process(Command::Deposit {
            subaccount: 2,
            amount_quote_minor: 100_000_000,
        });
        let legs = vec![RfqLegCommand {
            symbol: "BTC-PERP".into(),
            side: Side::Bid,
            qty_lots: 50,
        }];
        let ev = e.process(Command::RfqCreate {
            taker: 1,
            legs,
            counterparties: vec![],
            min_total_cost_quote_minor: None,
            max_total_cost_quote_minor: None,
            ttl_ms: 600_000,
            now: T0,
        });
        assert!(matches!(ev[0], Event::RfqCreated { .. }));
        let rfq_id = 1; // first id
        let ev = e.process(Command::RfqQuote {
            maker: 2,
            rfq_id,
            leg_prices_ticks: vec![80_000],
            ttl_ms: 600_000,
            now: T0,
        });
        assert!(matches!(ev[0], Event::RfqQuoted { .. }));
        // A second, worse quote loses.
        e.process(Command::RfqQuote {
            maker: 2,
            rfq_id,
            leg_prices_ticks: vec![80_500],
            ttl_ms: 600_000,
            now: T0,
        });
        let ev = e.process(Command::RfqExecute {
            taker: 1,
            rfq_id,
            quote_id: 1,
            now: T0,
        });
        let settled = ev.iter().any(|x| matches!(x, Event::RfqSettled { .. }));
        assert!(settled, "rfq executes: {ev:?}");
        assert_eq!(e.account(1).map(|a| a.lots_of("BTC-PERP")), Some(50));
        assert_eq!(e.account(2).map(|a| a.lots_of("BTC-PERP")), Some(-50));
        // The taker paid a fee; the maker paid zero (Paradigm economics).
        let taker_fee = e.account(1).map(|a| a.fees_paid_quote_minor);
        let maker_fee = e.account(2).map(|a| a.fees_paid_quote_minor);
        assert!(taker_fee.is_some_and(|f| f > 0));
        assert_eq!(maker_fee, Some(0));
        // RFQ is filled; executing again fails.
        let ev = e.process(Command::RfqExecute {
            taker: 1,
            rfq_id,
            quote_id: 2,
            now: T0,
        });
        assert!(matches!(ev[0], Event::RfqRejected { .. }));
    }

    #[test]
    fn rfq_directed_flow_is_private() {
        use crate::command::RfqLegCommand;
        let mut e = engine_with_market();
        seed_oracle(&mut e, T0, 8_000_000);
        e.process(Command::Deposit {
            subaccount: 1,
            amount_quote_minor: 100_000_000,
        });
        e.process(Command::Deposit {
            subaccount: 2,
            amount_quote_minor: 100_000_000,
        });
        e.process(Command::Deposit {
            subaccount: 3,
            amount_quote_minor: 100_000_000,
        });
        e.process(Command::RfqCreate {
            taker: 1,
            legs: vec![RfqLegCommand {
                symbol: "BTC-PERP".into(),
                side: Side::Bid,
                qty_lots: 10,
            }],
            counterparties: vec![2],
            min_total_cost_quote_minor: None,
            max_total_cost_quote_minor: None,
            ttl_ms: 600_000,
            now: T0,
        });
        // Maker 3 is not a directed counterparty.
        let ev = e.process(Command::RfqQuote {
            maker: 3,
            rfq_id: 1,
            leg_prices_ticks: vec![80_000],
            ttl_ms: 600_000,
            now: T0,
        });
        assert!(matches!(ev[0], Event::RfqRejected { .. }));
        // Maker 2 is.
        let ev = e.process(Command::RfqQuote {
            maker: 2,
            rfq_id: 1,
            leg_prices_ticks: vec![80_000],
            ttl_ms: 600_000,
            now: T0,
        });
        assert!(matches!(ev[0], Event::RfqQuoted { .. }));
    }

    #[test]
    fn mmp_trips_cancels_and_freezes() {
        let mut e = engine_with_market();
        seed_oracle(&mut e, T0, 8_000_000);
        e.process(Command::Deposit {
            subaccount: 1,
            amount_quote_minor: 100_000_000,
        });
        e.process(Command::Deposit {
            subaccount: 2,
            amount_quote_minor: 100_000_000,
        });
        // Very tight protection: 50 lots of fills trips it.
        e.process(Command::SetMmp {
            subaccount: 2,
            base_symbol: "BTC".into(),
            interval_ms: 60_000,
            frozen_time_ms: 120_000,
            amount_limit_lots: 50,
            delta_limit_lots: 0,
            now: T0,
        });
        e.process(Command::Place {
            request: OrderRequest::limit(2, "BTC-PERP", Side::Ask, 80_000, 50),
            now: T0,
        });
        let ev = e.process(Command::Place {
            request: OrderRequest::limit(1, "BTC-PERP", Side::Bid, 80_000, 50),
            now: T0,
        });
        assert!(
            ev.iter().any(|x| matches!(x, Event::MmpTripped { .. })),
            "{ev:?}"
        );
        // Frozen: the maker's next order is rejected while frozen.
        let ev = e.process(Command::Place {
            request: OrderRequest::limit(2, "BTC-PERP", Side::Ask, 80_000, 1),
            now: T0,
        });
        assert!(matches!(ev[0], Event::OrderRejection(_)));
    }

    #[test]
    fn cancel_on_disconnect_pulls_resting_orders() {
        let mut e = engine_with_market();
        seed_oracle(&mut e, T0, 8_000_000);
        e.process(Command::Deposit {
            subaccount: 1,
            amount_quote_minor: 10_000_000,
        });
        e.process(Command::SetCod {
            subaccount: 1,
            enabled: true,
            now: T0,
        });
        e.process(Command::Place {
            request: OrderRequest::limit(1, "BTC-PERP", Side::Bid, 79_000, 10),
            now: T0,
        });
        assert_eq!(e.account(1).map(|a| a.open_orders.len()), Some(1));
        let ev = e.process(Command::SessionDropped {
            subaccount: 1,
            now: T0,
        });
        assert!(ev
            .iter()
            .any(|x| matches!(x, Event::SessionDisconnected { .. })));
        assert_eq!(e.account(1).map(|a| a.open_orders.len()), Some(0));
        // Without CoD enabled, a drop leaves orders resting.
        e.process(Command::SetCod {
            subaccount: 1,
            enabled: false,
            now: T0,
        });
        e.process(Command::Place {
            request: OrderRequest::limit(1, "BTC-PERP", Side::Bid, 79_000, 10),
            now: T0,
        });
        e.process(Command::SessionDropped {
            subaccount: 1,
            now: T0,
        });
        assert_eq!(e.account(1).map(|a| a.open_orders.len()), Some(1));
    }

    #[test]
    fn internal_transfers_move_free_equity() {
        let mut e = engine_with_market();
        seed_oracle(&mut e, T0, 8_000_000);
        e.process(Command::Deposit {
            subaccount: 1,
            amount_quote_minor: 1_000_000,
        });
        e.process(Command::Deposit {
            subaccount: 2,
            amount_quote_minor: 0,
        });
        let ev = e.process(Command::Transfer {
            from: 1,
            to: 2,
            amount_quote_minor: 400_000,
            now: T0,
        });
        assert!(matches!(ev[0], Event::TransferExecuted { .. }));
        assert_eq!(e.account(1).map(|a| a.cash_quote_minor), Some(600_000));
        assert_eq!(e.account(2).map(|a| a.cash_quote_minor), Some(400_000));
        // Beyond free equity: rejected (all cash is margin-committed here? no
        // position — but 2M is more than the account holds).
        let ev = e.process(Command::Transfer {
            from: 1,
            to: 2,
            amount_quote_minor: 2_000_000,
            now: T0,
        });
        assert!(matches!(ev[0], Event::TransferRejected { .. }));
    }

    #[test]
    fn block_trades_register_and_print_late() {
        let mut e = engine_with_market();
        seed_oracle(&mut e, T0, 8_000_000);
        e.process(Command::Deposit {
            subaccount: 1,
            amount_quote_minor: 100_000_000,
        });
        e.process(Command::Deposit {
            subaccount: 2,
            amount_quote_minor: 100_000_000,
        });
        let ev = e.process(Command::BlockTrade {
            taker: 1,
            maker: 2,
            legs: vec![("BTC-PERP".into(), Side::Bid, 20, 80_000)],
            now: T0,
        });
        assert!(matches!(ev[0], Event::BlockRegistered { .. }));
        // Block settled as a venue trade immediately...
        assert_eq!(e.account(1).map(|a| a.lots_of("BTC-PERP")), Some(20));
        // ...but prints to the public tape only after the delay.
        let t1 = T0 + 500_000;
        e.process(Command::OracleUpdate {
            base_symbol: "BTC".into(),
            provider: "pyth".into(),
            ts: t1,
            price_quote_minor: 8_000_000,
        });
        let ev = e.process(Command::Tick { now: t1 });
        assert!(
            !ev.iter().any(|x| matches!(x, Event::BlockPrinted { .. })),
            "too early"
        );
        let t2 = T0 + poc_rfq::BlockLedger::DEFAULT_DELAY_MS + 1;
        e.process(Command::OracleUpdate {
            base_symbol: "BTC".into(),
            provider: "pyth".into(),
            ts: t2,
            price_quote_minor: 8_000_000,
        });
        let ev = e.process(Command::Tick { now: t2 });
        assert!(
            ev.iter().any(|x| matches!(x, Event::BlockPrinted { .. })),
            "prints after delay"
        );
    }

    #[test]
    fn vol_surface_governs_mark_iv() {
        let mut e = Engine::new(config());
        e.register_instrument(Instrument::Option(everlasting_call()));
        seed_oracle(&mut e, T0, 8_000_000);
        // Default anchor IV is 5500 bps (55%) when no config IV is set.
        let before = e.vol_surface_view().mark_iv_bps("BTC-EVER-80000-C");
        assert_eq!(before, Some(5_500));
        e.process(Command::Deposit {
            subaccount: 1,
            amount_quote_minor: 100_000_000,
        });
        e.process(Command::Deposit {
            subaccount: 2,
            amount_quote_minor: 100_000_000,
        });
        // A two-sided book at a rich premium pushes the blended IV toward
        // the book's implied level, clamped by the per-sweep move limit.
        e.process(Command::Place {
            request: OrderRequest::limit(2, "BTC-EVER-80000-C", Side::Ask, 12_000, 100),
            now: T0,
        });
        e.process(Command::Place {
            request: OrderRequest::limit(1, "BTC-EVER-80000-C", Side::Bid, 11_900, 100),
            now: T0,
        });
        for i in 0..5 {
            let t = T0 + i * 1_000;
            e.process(Command::OracleUpdate {
                base_symbol: "BTC".into(),
                provider: "pyth".into(),
                ts: t,
                price_quote_minor: 8_000_000,
            });
            e.process(Command::Tick { now: t });
        }
        let after = e.vol_surface_view().mark_iv_bps("BTC-EVER-80000-C");
        // The mark IV moved toward the richer book, but within the clamp.
        let after = after.unwrap_or(5_500);
        assert!(after >= 5_500, "richer book pulls IV up: {after}");
        assert!(after < 20_000, "sanity band holds: {after}");
    }

    #[test]
    fn greeks_view_reports_option_positions() {
        let mut e = Engine::new(config());
        e.register_instrument(Instrument::Option(everlasting_call()));
        seed_oracle(&mut e, T0, 8_000_000);
        e.process(Command::Deposit {
            subaccount: 1,
            amount_quote_minor: 100_000_000,
        });
        e.process(Command::Deposit {
            subaccount: 2,
            amount_quote_minor: 100_000_000,
        });
        let ev = e.process(Command::Place {
            request: OrderRequest::limit(2, "BTC-EVER-80000-C", Side::Ask, 10_000, 100),
            now: T0,
        });
        assert!(
            ev.iter().any(|x| matches!(x, Event::OrderResting { .. })),
            "ASKREST {ev:?}"
        );
        let ev = e.process(Command::Place {
            request: OrderRequest::limit(1, "BTC-EVER-80000-C", Side::Bid, 10_000, 100),
            now: T0,
        });
        assert!(
            ev.iter().any(|x| matches!(x, Event::TradeExecuted(_))),
            "BIDTRADE {ev:?}"
        );
        let greeks = e.greeks_view(1).unwrap_or_default();
        assert_eq!(greeks.len(), 1);
        let (_, delta, _vega) = greeks[0].clone();
        assert!(delta > 0, "long ATM-ish call has positive delta: {delta}");
    }

    #[test]
    fn new_features_replay_bit_for_bit() {
        let mut e = engine_with_market();
        e.register_instrument(Instrument::Option(everlasting_call()));
        seed_oracle(&mut e, T0, 8_000_000);
        e.process(Command::Deposit {
            subaccount: 1,
            amount_quote_minor: 100_000_000,
        });
        e.process(Command::Deposit {
            subaccount: 2,
            amount_quote_minor: 100_000_000,
        });
        // CLOB trade on the perp.
        e.process(Command::Place {
            request: OrderRequest::limit(2, "BTC-PERP", Side::Ask, 80_100, 30),
            now: T0,
        });
        e.process(Command::Place {
            request: OrderRequest::limit(1, "BTC-PERP", Side::Bid, 80_100, 30),
            now: T0,
        });
        // Option trade on the everlasting market.
        e.process(Command::Place {
            request: OrderRequest::limit(2, "BTC-EVER-80000-C", Side::Ask, 10_000, 100),
            now: T0,
        });
        e.process(Command::Place {
            request: OrderRequest::limit(1, "BTC-EVER-80000-C", Side::Bid, 10_000, 100),
            now: T0,
        });
        // RFQ flow.
        use crate::command::RfqLegCommand;
        e.process(Command::RfqCreate {
            taker: 1,
            legs: vec![RfqLegCommand {
                symbol: "BTC-PERP".into(),
                side: Side::Bid,
                qty_lots: 20,
            }],
            counterparties: vec![],
            min_total_cost_quote_minor: None,
            max_total_cost_quote_minor: None,
            ttl_ms: 600_000,
            now: T0,
        });
        e.process(Command::RfqQuote {
            maker: 2,
            rfq_id: 1,
            leg_prices_ticks: vec![80_200],
            ttl_ms: 600_000,
            now: T0,
        });
        e.process(Command::RfqExecute {
            taker: 1,
            rfq_id: 1,
            quote_id: 1,
            now: T0,
        });
        // A roll interval + ticks.
        let t1 = T0 + 61_000;
        for provider in ["pyth", "chainlink"] {
            e.process(Command::OracleUpdate {
                base_symbol: "BTC".into(),
                provider: provider.into(),
                ts: t1,
                price_quote_minor: 8_000_000,
            });
        }
        e.process(Command::Tick { now: t1 });

        // Replay: same journal into a fresh engine, identical state.
        let replayed = Engine::replay(config(), e.journal());
        for sub in [1_u64, 2] {
            let a = e.account(sub).cloned();
            let b = replayed.account(sub).cloned();
            assert_eq!(a, b, "subaccount {sub} matches after replay");
        }
        assert_eq!(
            e.vol_surface_view().mark_iv_bps("BTC-EVER-80000-C"),
            replayed.vol_surface_view().mark_iv_bps("BTC-EVER-80000-C"),
            "vol surface replay is exact"
        );
        assert_eq!(e.insurance().balance(), replayed.insurance().balance());
        assert_eq!(e.stats().trades, replayed.stats().trades);
        assert_eq!(
            e.stats().funding_intervals,
            replayed.stats().funding_intervals
        );
    }

    #[test]
    fn determinism_and_replay() {
        let script = |e: &mut Engine| {
            e.register_instrument(Instrument::Perp(PerpMarket::default()));
            let opt = OptionMarket {
                expiry_ts_ms: T0 + 7_200_000,
                ..OptionMarket::default()
            };
            e.register_instrument(Instrument::Option(opt));
            e.process(Command::Deposit {
                subaccount: 1,
                amount_quote_minor: 50_000_000,
            });
            e.process(Command::Deposit {
                subaccount: 2,
                amount_quote_minor: 50_000_000,
            });
            e.process(Command::Deposit {
                subaccount: 3,
                amount_quote_minor: 50_000_000,
            });

            let mut t = T0;
            for round in 0..10 {
                for provider in ["pyth", "chainlink"] {
                    e.process(Command::OracleUpdate {
                        base_symbol: "BTC".into(),
                        provider: provider.into(),
                        ts: t,
                        price_quote_minor: 8_000_000 + u128::from(round) * 25_000,
                    });
                }
                // Round-robin maker/taker across all accounts.
                let price = 80_000 + round * 5;
                e.process(Command::Place {
                    request: OrderRequest::limit(
                        1 + (round % 3),
                        "BTC-PERP",
                        Side::Ask,
                        price + 2,
                        30,
                    ),
                    now: t,
                });
                e.process(Command::Place {
                    request: OrderRequest::limit(
                        1 + ((round + 1) % 3),
                        "BTC-PERP",
                        Side::Bid,
                        price + 2,
                        20,
                    ),
                    now: t,
                });
                e.process(Command::Tick { now: t });
                t += 3_600_000;
            }
            // An option trade and its expiry.
            e.process(Command::Place {
                request: OrderRequest::limit(2, "BTC-19800-80000-C", Side::Ask, 38_000, 40),
                now: t,
            });
            e.process(Command::Place {
                request: OrderRequest::limit(1, "BTC-19800-80000-C", Side::Bid, 38_000, 40),
                now: t,
            });
            for provider in ["pyth", "chainlink"] {
                e.process(Command::OracleUpdate {
                    base_symbol: "BTC".into(),
                    provider: provider.into(),
                    ts: t,
                    price_quote_minor: 8_500_000,
                });
            }
            e.process(Command::Tick {
                now: t + 7_200_000 + 1,
            });
        };

        // Run 1: live.
        let mut a = Engine::new(config());
        script(&mut a);
        // Run 2: identical commands.
        let mut b = Engine::new(config());
        script(&mut b);
        assert_eq!(
            a.journal(),
            b.journal(),
            "identical command streams must journal identically"
        );

        // Run 3: replay the journal into a fresh engine.
        let c = Engine::replay(config(), a.journal());
        // Replay must reproduce the same books, accounts, and balances.
        for sub in 1..=3 {
            let av = a.account_view(sub);
            let cv = c.account_view(sub);
            assert_eq!(av, cv, "account {sub} must replay exactly");
        }
        assert_eq!(a.stats(), c.stats(), "stats must replay exactly");
        assert_eq!(a.journal().len(), c.journal().len());
        assert_eq!(
            a.stats().trades,
            a.journal()
                .iter()
                .filter(|e| matches!(e, Event::TradeExecuted(_)))
                .count() as u64
        );
    }

    #[test]
    fn self_trade_prevention_cancels_newest() {
        let mut e = engine_with_market();
        seed_oracle(&mut e, T0, 8_000_000);
        e.process(Command::Deposit {
            subaccount: 1,
            amount_quote_minor: 100_000_000,
        });
        e.process(Command::Place {
            request: OrderRequest::limit(1, "BTC-PERP", Side::Ask, 80_000, 50),
            now: T0,
        });
        // Same account crosses its own order: STP kills the taker, no
        // trade, the resting maker is untouched — and no position exists
        // (positions come only from fills).
        let ev = e.process(Command::Place {
            request: OrderRequest::limit(1, "BTC-PERP", Side::Bid, 80_000, 20),
            now: T0,
        });
        assert!(
            ev.iter().all(|x| !matches!(x, Event::TradeExecuted(_))),
            "no self trade: {ev:?}"
        );
        assert_eq!(e.account(1).map(|a| a.lots_of("BTC-PERP")), Some(0));
        assert_eq!(e.book("BTC-PERP").map(|b| b.open_order_count()), Some(1));
    }
}
