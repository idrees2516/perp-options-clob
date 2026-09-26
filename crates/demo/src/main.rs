//! # poc-demo — the exchange, end to end
//!
//! A scripted session that exercises every subsystem of the engine in a
//! realistic order: listing, market making, taker flow, funding, oracle
//! defense, liquidation, option settlement — with a full accounting
//! summary at the end.
//!
//! ```text
//! cargo run -p poc-demo
//! ```

use poc_core::{Instrument, OptionKind, OptionMarket, PerpMarket, Side};
use poc_engine::{Command, Engine, EngineConfig, Event, OrderRequest};

/// Quote minor → `$x,xxx.xx` display.
fn usd(minor: i128) -> String {
    let sign = if minor < 0 { "-" } else { "" };
    let abs = minor.unsigned_abs();
    let whole = abs / 100;
    let cents = abs % 100;
    format!("{sign}${whole}.{cents:02}")
}

/// Basis points → signed percent string.
fn bps_display(bps: i64) -> String {
    format!("{:+.4}%", bps as f64 / 100.0)
}

/// Deterministic pseudo-random for the session.
struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

const T0: u64 = 1_700_000_000_000; // a plausible epoch ms
const HOUR: u64 = 3_600_000;
const PROVIDERS: [&str; 3] = ["pyth", "chainlink", "apis3"];

struct Session {
    engine: Engine,
    now: u64,
    price: u128,
}

impl Session {
    fn feed_oracle(&mut self, price: u128) {
        // Providers agree with ±2bp jitter — realistic independent feeds.
        for (i, provider) in PROVIDERS.iter().enumerate() {
            let jitter = match i {
                0 => price.saturating_sub(price / 5_000),
                1 => price.saturating_add(price / 5_000),
                _ => price,
            };
            self.engine.process(Command::OracleUpdate {
                base_symbol: "BTC".into(),
                provider: (*provider).into(),
                ts: self.now,
                price_quote_minor: jitter,
            });
        }
        self.price = price;
    }

    fn tick(&mut self) -> Vec<Event> {
        self.engine.process(Command::Tick { now: self.now })
    }

    fn walk_price(&mut self, target: u128, steps: u64) -> Vec<Event> {
        // Move in sub-quarantine steps (< 4% each).
        let mut current = self.price;
        let mut last = Vec::new();
        for _ in 0..steps {
            let diff = target as i128 - current as i128;
            let step = diff / 4; // ≤ 25% of remaining, bounded by steps
            let next = (current as i128 + step).max(1) as u128;
            // Clamp each hop to ±3%.
            let clamped = if next > current {
                current.saturating_add((current / 33).min(next - current))
            } else {
                current.saturating_sub((current / 33).min(current - next))
            };
            self.now += 10_000;
            self.feed_oracle(clamped);
            current = clamped;
            last = self.tick();
            if clamped == target {
                break;
            }
        }
        // Top up to the exact target with micro steps.
        while current != target {
            let hop = if target > current {
                current.saturating_add((target - current).min(current / 50))
            } else {
                current.saturating_sub((current - target).min(current / 50))
            };
            current = hop;
            self.now += 10_000;
            self.feed_oracle(hop);
            last = self.tick();
        }
        last
    }
}

fn main() {
    println!("╔══════════════════════════════════════════════════════════════════╗");
    println!("║  perp-options-clob — end-to-end engine session                   ║");
    println!("║  BTC-PERP · BTC-80000-C · portfolio margin · oracle defense      ║");
    println!("╚══════════════════════════════════════════════════════════════════╝");
    println!();

    // ------------------------------------------------------------------
    // 1. Genesis: list markets, seed oracles and participants.
    // ------------------------------------------------------------------
    let config = EngineConfig {
        reward_per_interval_quote_minor: 50_000, // $500/h maker rewards
        reward_interval_ms: HOUR,
        insurance_seed_quote_minor: 10_000_000, // $100k seeded fund
        option_ivs: [("BTC-80000-C".to_string(), 0.55)].into_iter().collect(),
        ..EngineConfig::default()
    };

    let mut s = Session {
        engine: Engine::new(config),
        now: T0,
        price: 8_000_000, // $80,000.00
    };

    let perp = PerpMarket::default(); // $1 ticks, 0.001 BTC lots, 8h funding
                                      // 80k call, 0.01 lots, $0.50 ticks, expiring in 30 days.
    let call = OptionMarket {
        expiry_ts_ms: T0 + 30 * 24 * HOUR,
        ..OptionMarket::default()
    };
    s.engine.register_instrument(Instrument::Perp(perp));
    s.engine
        .register_instrument(Instrument::Option(call.clone()));

    // Participants: 2 market makers, 3 directional traders, 1 degen.
    // Accounts (quote minor): MM=1,2 ($5M) T=3,4,5 ($2M) D=6 ($45k).
    for (sub, cash) in [
        (1_u64, 500_000_000_u128),
        (2, 500_000_000),
        (3, 200_000_000),
        (4, 200_000_000),
        (5, 200_000_000),
        (6, 4_500_000),
    ] {
        s.engine.process(Command::Deposit {
            subaccount: sub,
            amount_quote_minor: cash,
        });
    }
    s.feed_oracle(s.price);
    s.tick();

    println!("[genesis] markets listed, 6 accounts funded, oracle quorum live");
    println!();

    // ------------------------------------------------------------------
    // 2. Market makers quote two-sided books.
    // ------------------------------------------------------------------
    println!(
        "[making]  MM-1 and MM-2 quote BTC-PERP around ${}",
        s.price / 100
    );
    let (bid, ask) = (79_900_u64, 80_100_u64);
    for (mm, (b, a)) in [(1_u64, (bid, ask)), (2, (79_950, 80_050))] {
        s.engine.process(Command::Place {
            request: OrderRequest::limit(mm, "BTC-PERP", Side::Bid, b, 4_000),
            now: s.now,
        });
        s.engine.process(Command::Place {
            request: OrderRequest::limit(mm, "BTC-PERP", Side::Ask, a, 4_000),
            now: s.now,
        });
    }
    // MM-2 also quotes the option near its theoretical premium.
    let opt_mark = 9_600; // ticks of $0.50 → ~$4.8k premium per BTC
    s.engine.process(Command::Place {
        request: OrderRequest::limit(2, "BTC-19800-80000-C", Side::Bid, opt_mark - 100, 50),
        now: s.now,
    });
    s.engine.process(Command::Place {
        request: OrderRequest::limit(2, "BTC-19800-80000-C", Side::Ask, opt_mark + 100, 50),
        now: s.now,
    });
    println!(
        "          book: bid {} / ask {} (+ option quotes at ±$50 around mark)",
        bid, ask
    );
    println!();

    // ------------------------------------------------------------------
    // 3. Taker flow: directional traders lift offers.
    // ------------------------------------------------------------------
    println!("[flow]    takers arrive (deterministic pseudo-random sizes)");
    let mut rng = XorShift(0xDEAD_BEEF_CAFE_F00D);
    let mut trades_seen = 0_usize;
    for round in 0..12 {
        let taker = 3 + rng.next() % 3; // accounts 3..5
        let side = if rng.next() % 2 == 0 {
            Side::Bid
        } else {
            Side::Ask
        };
        let qty = 5 + rng.next() % 40; // 5..45 lots
        let price = match side {
            Side::Bid => 80_100 + (round % 3) as u64,
            Side::Ask => 79_900 - (round % 3) as u64,
        };
        let events = s.engine.process(Command::Place {
            request: OrderRequest::limit(taker, "BTC-PERP", side, price, qty),
            now: s.now,
        });
        trades_seen += events
            .iter()
            .filter(|e| matches!(e, Event::TradeExecuted(_)))
            .count();
    }
    // A directional options buyer: trader-3 buys 1.0 BTC of calls.
    let events = s.engine.process(Command::Place {
        request: OrderRequest::limit(3, "BTC-19800-80000-C", Side::Bid, opt_mark + 100, 100),
        now: s.now,
    });
    trades_seen += events
        .iter()
        .filter(|e| matches!(e, Event::TradeExecuted(_)))
        .count();
    println!("          {trades_seen} trades crossed the book");
    let stats = s.engine.stats();
    println!(
        "          volume: {} lots, {} notional",
        stats.lots_traded,
        usd(stats.notional_traded as i128)
    );
    println!();

    // ------------------------------------------------------------------
    // 4. Rewards: the hourly maker pool settles pro-rata.
    // ------------------------------------------------------------------
    s.now += HOUR;
    s.feed_oracle(s.price);
    let events = s.tick();
    let rewards: Vec<&Event> = events
        .iter()
        .filter(|e| matches!(e, Event::Reward(_)))
        .collect();
    println!(
        "[rewards] hourly pool settled: {} payments to makers",
        rewards.len()
    );
    for e in &rewards {
        if let Event::Reward(p) = e {
            println!(
                "          MM-{} earned {}",
                p.subaccount,
                usd(p.amount_quote_minor as i128)
            );
        }
    }
    println!();

    // ------------------------------------------------------------------
    // 5. Oracle defense: a rogue print gets quarantined.
    // ------------------------------------------------------------------
    println!(
        "[oracle]  a compromised feed prints ${} — deviation quarantine engages",
        160_000
    );
    s.engine.process(Command::OracleUpdate {
        base_symbol: "BTC".into(),
        provider: "compromised".into(),
        ts: s.now,
        price_quote_minor: 16_000_000,
    });
    let spot = s
        .engine
        .market_state()
        .spots
        .iter()
        .find(|(b, _)| b == "BTC")
        .and_then(|(_, p)| *p);
    println!(
        "          mark unmoved at ${} (median of healthy providers)",
        spot.unwrap_or(0) / 100
    );
    println!();

    // ------------------------------------------------------------------
    // 6. Funding: the 8h interval settles (mark ≈ index → small premium).
    // ------------------------------------------------------------------
    println!("[funding] advancing to the 8-hour boundary...");
    let mut funding_events: Vec<Event> = Vec::new();
    while s.now < T0 + 8 * HOUR {
        s.now += HOUR / 2;
        s.feed_oracle(s.price);
        funding_events = s.tick();
        if funding_events
            .iter()
            .any(|e| matches!(e, Event::Funding(_)))
        {
            break;
        }
    }
    for e in &funding_events {
        if let Event::Funding(f) = e {
            println!(
                "          BTC-PERP rate {} — longs pay shorts (BitMEX premium + interest model)",
                bps_display(f.rate_bps)
            );
        }
    }
    let f3 = s
        .engine
        .account_view(3)
        .map(|v| v.funding_received_quote_minor);
    println!(
        "          trader-3 funding: {}",
        f3.map_or("n/a".into(), |x| usd(-x))
    );
    println!();

    // ------------------------------------------------------------------
    // 7. Liquidation: the degen's 20x long gets crashed into.
    // ------------------------------------------------------------------
    println!("[risk]    degen-6 opens max leverage at the offer...");
    let events = s.engine.process(Command::Place {
        request: OrderRequest::limit(6, "BTC-PERP", Side::Bid, 80_100, 8_000),
        now: s.now,
    });
    let filled: u64 = events
        .iter()
        .filter_map(|e| match e {
            Event::TradeExecuted(t) => Some(t.qty_lots),
            _ => None,
        })
        .sum();
    println!(
        "          filled {filled} lots ({} BTC)",
        filled as f64 / 1000.0
    );
    let view = s.engine.account_view(6);
    if let Some(v) = view {
        let leverage = v.summary.initial_quote_minor.max(1) as f64
            / v.summary.equity_quote_minor.max(1) as f64;
        println!(
            "          equity {}, initial margin {} → {:.1}x effective",
            usd(v.summary.equity_quote_minor),
            usd(v.summary.initial_quote_minor as i128),
            leverage
        );
    }

    println!("          the market sells off to $71,200 over several prints...");
    let mut crash_events = s.walk_price(7_120_000, 14);
    // Keep ticking until the liquidation settles (or enough time passes).
    for _ in 0..10 {
        if crash_events
            .iter()
            .any(|e| matches!(e, Event::Liquidation(_) | Event::Adl(_)))
        {
            break;
        }
        s.now += 10_000;
        s.feed_oracle(s.price);
        crash_events = s.tick();
    }

    let stats = s.engine.stats();
    let view = s.engine.account_view(6);
    if let Some(v) = view {
        println!(
            "          degen-6 after liquidation: equity {}, position {} lots, health={:?}",
            usd(v.summary.equity_quote_minor),
            v.positions
                .iter()
                .find(|(sym, _, _)| sym == "BTC-PERP")
                .map(|&(_, l, _)| l)
                .unwrap_or(0),
            poc_margin::Health::classify(&v.summary)
        );
    }
    println!(
        "          liquidations executed: {}, auto-deleverages: {}, insurance fund balance {}",
        stats.liquidations,
        stats.adls,
        usd(stats.insurance_balance)
    );
    println!();

    // ------------------------------------------------------------------
    // 8. Settlement: the option expires 30 days out, spot at $86k.
    // ------------------------------------------------------------------
    println!("[expiry]  advancing 30 days; spot rallies to $86,000...");
    s.now = T0 + 30 * 24 * HOUR - 10 * HOUR;
    // Rally first (still before the expiry boundary), then cross it.
    s.walk_price(8_600_000, 12);
    let mut expiry_events: Vec<Event> = Vec::new();
    while s.now < T0 + 30 * 24 * HOUR {
        s.now += HOUR;
        s.feed_oracle(8_600_000);
        expiry_events = s.tick();
    }
    for e in &expiry_events {
        if let Event::OptionExpiry(settled) = e {
            println!(
                "          account {} settled {} lots at {} → payout {}",
                settled.subaccount,
                settled.signed_lots,
                usd(settled.settlement_quote_minor as i128),
                usd(settled.payout_quote_minor)
            );
        }
    }
    println!(
        "          (80k calls expire {} ITM — cash-settled on the 30-min TWAP)",
        usd((8_600_000_i128 - 8_000_000) / 100 * 100)
    );
    println!();

    // ------------------------------------------------------------------
    // 9. Accounting: the final ledger.
    // ------------------------------------------------------------------
    let stats = s.engine.stats();
    println!("╔══════════════════════════════════════════════════════════════════╗");
    println!("║  session summary                                                 ║");
    println!("╚══════════════════════════════════════════════════════════════════╝");
    println!(
        "  trades: {}   lots: {}   notional: {}",
        stats.trades,
        stats.lots_traded,
        usd(stats.notional_traded as i128)
    );
    println!("  events journaled: {}", stats.events);
    println!();
    println!("  fee revenue routed (60/30/10 house/insurance/buyback):");
    println!("    house      {:>12}", usd(stats.revenue.house as i128));
    println!(
        "    insurance  {:>12}",
        usd(stats.revenue.insurance as i128)
    );
    println!("    buyback    {:>12}", usd(stats.revenue.buyback as i128));
    println!();
    println!("  insurance fund balance: {}", usd(stats.insurance_balance));
    println!("  funding intervals settled: {}", stats.funding_intervals);
    println!("  options settled at expiry: {}", stats.options_settled);
    println!("  liquidation closures: {}", stats.liquidations);
    println!("  auto-deleverages: {}", stats.adls);
    println!();
    println!("  accounts:");
    for sub in 1..=6_u64 {
        if let Some(v) = s.engine.account_view(sub) {
            let role = match sub {
                1 => "MM-1",
                2 => "MM-2",
                3 => "T-3",
                4 => "T-4",
                5 => "T-5",
                _ => "degen-6",
            };
            let pos: Vec<String> = v
                .positions
                .iter()
                .map(|(_, lots, _)| format!("{}{}", if *lots > 0 { "+" } else { "" }, lots))
                .collect();
            println!(
                "    {} equity {:>12}  cash {:>12}  fees paid {:>9}  positions [{}]",
                role,
                usd(v.summary.equity_quote_minor),
                usd(v.cash_quote_minor),
                usd(v.fees_paid_quote_minor as i128),
                pos.join(", ")
            );
        }
    }
    println!();

    // ------------------------------------------------------------------
    // 10. Determinism: replay the journal into a fresh engine.
    // ------------------------------------------------------------------
    let replayed = Engine::replay(s.engine.config().clone(), s.engine.journal());
    let identical = (1..=6_u64).all(|sub| s.engine.account_view(sub) == replayed.account_view(sub))
        && s.engine.stats() == replayed.stats();
    println!(
        "[audit]   journal replay reproduces the state: {}",
        if identical { "YES ✓" } else { "NO ✗" }
    );
    let _ = OptionKind::Call; // silence unused import in trimmed builds
}
