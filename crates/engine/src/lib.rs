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

pub mod command;
pub mod engine;
pub mod event;
pub mod sweep;

pub use command::{Command, OrderRequest};
pub use engine::{Engine, EngineConfig, EngineStats};
pub use event::{
    AccountView, AdlExecuted, BookView, Event, FundingPaid, FundingSettled, LiquidationExecuted,
    LiquidityObservation, MarketStateView, OptionSettled, OrderCloseReason, OrderRejected,
    RewardPaid, Trade,
};

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
