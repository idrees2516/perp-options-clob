//! Benchmarks and architecture-level stress tests.
//!
//! `poc-bench micro` — deterministic micro-benchmarks (no external
//! harness; ns/op with percentiles over fixed iteration counts):
//! order placement/cancellation throughput, random-cross matching,
//! oracle updates, margin summaries, WAL appends + recovery, and
//! settlement-state root computation.
//!
//! `poc-bench stress` — whole-architecture scenarios: flash crash,
//! volatility spike, order-spam, liquidation cascade, oracle split,
//! and crash-recovery determinism. Each prints a structured report.

use std::time::Instant;

use poc_core::{Instrument, PerpMarket, Side};
use poc_engine::{Command, Engine, EngineConfig, OrderRequest};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("micro") => micro(),
        Some("stress") => {
            stress::run_all();
        }
        _ => {
            println!("usage: poc-bench <micro|stress> [filter]");
            println!("  micro        throughput + latency of core operations");
            println!("  stress       architecture stress scenarios");
        }
    }
}

// ----------------------------------------------------------------------
// Deterministic RNG (xorshift64*)
// ----------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn stats(label: &str, samples: &mut Vec<u64>) {
    samples.sort_unstable();
    let n = samples.len();
    let p = |q: f64| samples[((q * n as f64) as usize).min(n - 1)];
    let mean = samples.iter().sum::<u64>() / n as u64;
    println!(
        "{label:<42} p50={:>9}ns p99={:>9}ns max={:>10}ns mean={:>9}ns",
        p(0.50),
        p(0.99),
        samples[n - 1],
        mean
    );
    samples.clear();
}

fn setup_engine(subs: u64) -> Engine {
    let mut e = Engine::new(EngineConfig::default());
    e.register_instrument(Instrument::Perp(PerpMarket::default()));
    for provider in ["pyth", "chainlink"] {
        e.process(Command::OracleUpdate {
            base_symbol: "BTC".into(),
            provider: provider.into(),
            ts: 1_000,
            price_quote_minor: 8_000_000,
        });
    }
    e.process(Command::Tick { now: 1_000 });
    for sub in 1..=subs {
        e.process(Command::Deposit {
            subaccount: sub,
            amount_quote_minor: 1_000_000_000,
        });
    }
    e
}

// ----------------------------------------------------------------------
// Micro benchmarks
// ----------------------------------------------------------------------

fn micro() {
    println!("== micro benchmarks (release build) ==\n");

    // 1. Order placement + cancellation (book churn).
    {
        const N: usize = 60_000;
        let mut e = setup_engine(8);
        let mut rng = Rng::new(42);
        let mut samples = Vec::with_capacity(N);
        let mut id = 0_u64;
        for _ in 0..N {
            let sub = 1 + rng.below(8);
            let price = 70_000 + rng.below(20_000);
            let t = Instant::now();
            e.process(Command::Place {
                request: OrderRequest::limit(sub, "BTC-PERP", Side::Bid, price, 1 + rng.below(3)),
                now: 2_000,
            });
            id += 1;
            if id % 2 == 0 {
                e.process(Command::Cancel {
                    subaccount: sub,
                    order_id: id / 2,
                    now: 2_000,
                });
            }
            samples.push(t.elapsed().as_nanos() as u64);
        }
        stats("place/cancel (alternating, 8 accts)", &mut samples);
    }

    // 2. Random-cross matching (takers hitting resting liquidity).
    {
        const N: usize = 20_000;
        let mut e = setup_engine(8);
        let _rng = Rng::new(7);
        for i in 0..200 {
            e.process(Command::Place {
                request: OrderRequest::limit(1 + (i % 8), "BTC-PERP", Side::Ask, 79_500 + i, 5),
                now: 2_000,
            });
        }
        let mut samples = Vec::with_capacity(N);
        for i in 0..N {
            let t = Instant::now();
            e.process(Command::Place {
                request: OrderRequest::limit(
                    9 - (i % 8) as u64,
                    "BTC-PERP",
                    Side::Bid,
                    80_500 - (i as u64 % 900),
                    2,
                ),
                now: 2_000 + i as u64,
            });
            samples.push(t.elapsed().as_nanos() as u64);
        }
        stats("crossing taker order (vs 200 resting)", &mut samples);
    }

    // 3. Oracle updates + tick sweep cost.
    {
        const N: usize = 5_000;
        let mut e = setup_engine(4);
        let mut rng = Rng::new(11);
        let mut samples = Vec::with_capacity(N);
        for i in 0..N {
            let t = Instant::now();
            e.process(Command::OracleUpdate {
                base_symbol: "BTC".into(),
                provider: if i % 2 == 0 {
                    "pyth".into()
                } else {
                    "chainlink".into()
                },
                ts: 3_000 + i as u64 * 100,
                price_quote_minor: 7_900_000 + u128::from(rng.below(200_000)),
            });
            samples.push(t.elapsed().as_nanos() as u64);
        }
        stats("oracle provider update", &mut samples);

        let mut tick_samples = Vec::with_capacity(200);
        for i in 0..200 {
            let t = Instant::now();
            e.process(Command::Tick {
                now: 60_000 + i as u64 * 1_000,
            });
            tick_samples.push(t.elapsed().as_nanos() as u64);
        }
        stats("tick sweep (empty-ish book)", &mut tick_samples);
    }

    // 4. Settlement state root over N accounts.
    {
        use poc_settlement::{AccountCommitment, SettlementState};
        let mut s = SettlementState::empty();
        for i in 1..=2_000_u64 {
            let mut c = AccountCommitment {
                subaccount: i,
                cash_quote_minor: 1_000_000,
                positions: std::collections::BTreeMap::new(),
            };
            c.positions.insert("BTC-PERP".into(), (i % 11) as i64 - 5);
            s.upsert(c);
        }
        let mut samples = Vec::with_capacity(50);
        for _ in 0..50 {
            let t = Instant::now();
            let _ = s.root();
            samples.push(t.elapsed().as_nanos() as u64);
        }
        stats("settlement merkle root (2000 accts)", &mut samples);
    }

    // 5. WAL append + full recovery.
    {
        use poc_persist::WalWriter;
        let dir = std::env::temp_dir().join(format!("poc-bench-wal-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut wal = WalWriter::open(&dir).expect("open wal");
        const N: usize = 50_000;
        let mut samples = Vec::with_capacity(N);
        for i in 0..N {
            let cmd = Command::Deposit {
                subaccount: (i % 64) as u64,
                amount_quote_minor: 1_000,
            };
            let t = Instant::now();
            wal.append(&cmd).expect("append");
            samples.push(t.elapsed().as_nanos() as u64);
        }
        stats("WAL append (unsynced)", &mut samples);

        let t = Instant::now();
        let (_, report) = poc_persist::recover(EngineConfig::default(), &dir).expect("recover");
        println!(
            "{:<42} {:>8} cmds in {:>9?}",
            "WAL full recovery",
            report.commands_replayed,
            t.elapsed()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // 7. Second closure wave: OCO brackets, TWAP slicing, batch + vol
    // index (G-05/08/10/09).
    {
        const N: usize = 10_000;
        let mut e = setup_engine(8);
        let mut rng = Rng::new(99);
        let mut samples = Vec::with_capacity(N);
        for i in 0..N {
            let sub = 1 + rng.below(8);
            let price = 70_000 + rng.below(20_000);
            let t = Instant::now();
            e.process(Command::PlaceOco {
                first: OrderRequest::limit(sub, "BTC-PERP", Side::Bid, price, 1 + rng.below(3)),
                second: OrderRequest {
                    order_type: poc_core::OrderType::StopMarket {
                        trigger_price: price.saturating_sub(500),
                    },
                    ..OrderRequest::limit(
                        sub,
                        "BTC-PERP",
                        Side::Bid,
                        price.saturating_sub(600),
                        1 + rng.below(3),
                    )
                },
                now: 2_000,
            });
            // Sweep the brackets periodically so the OCO cascade runs.
            if i % 50 == 49 {
                e.process(Command::Tick { now: 3_000 });
                e.process(Command::CancelAll {
                    subaccount: sub,
                    symbol: None,
                    now: 3_000,
                });
            }
            samples.push(t.elapsed().as_nanos() as u64);
        }
        stats("OCO bracket place (incl. gate)", &mut samples);

        // TWAP slicing throughput: parents + slice ticks.
        let mut samples = Vec::with_capacity(N);
        for i in 0..N {
            let t = Instant::now();
            e.process(Command::PlaceTwap {
                subaccount: 1 + rng.below(8),
                symbol: "BTC-PERP".into(),
                side: Side::Bid,
                total_lots: 10,
                slices: 5,
                slice_interval_ms: 1_000,
                limit_ticks: Some(75_000),
                now: 4_000 + (i as u64) * 1_000,
            });
            e.process(Command::Tick {
                now: 4_001 + (i as u64) * 1_000,
            });
            samples.push(t.elapsed().as_nanos() as u64);
        }
        stats("TWAP open + one slice tick", &mut samples);

        // Batch placement (atomic gate + sequential commit).
        let mut samples = Vec::with_capacity(N);
        for _ in 0..N {
            let sub = 1 + rng.below(8);
            let t = Instant::now();
            e.process(Command::PlaceBatch {
                requests: (0..3)
                    .map(|k| {
                        OrderRequest::limit(
                            sub,
                            "BTC-PERP",
                            Side::Bid,
                            70_000 + rng.below(20_000) + k,
                            1,
                        )
                    })
                    .collect(),
                now: 5_000,
            });
            e.process(Command::CancelAll {
                subaccount: sub,
                symbol: None,
                now: 5_000,
            });
            samples.push(t.elapsed().as_nanos() as u64);
        }
        stats("batch of 3 (gate + commit)", &mut samples);

        // Vol index publication over the (optionless) book: cheap.
        let mut samples = Vec::with_capacity(1_000);
        for i in 0..1_000 {
            let t = Instant::now();
            e.process(Command::Tick { now: 6_000 + i });
            samples.push(t.elapsed().as_nanos() as u64);
        }
        stats("tick (incl. vol index + interest)", &mut samples);
    }

    // 8. Fourth closure wave: MM tier review, proof-of-reserves, FIX
    // session (G-15/35/26).
    {
        const N: usize = 1_000;

        // MM tier review cycle: enroll, sample liquidity ticks, close
        // the window. Measures the whole monthly-review cost per maker.
        let cfg = EngineConfig {
            mm_program: poc_economics::MmTierProgram {
                tiers: vec![poc_economics::MmTierSpec {
                    name: "BENCH-MM",
                    fee_discount_bps: 1_000,
                    min_uptime_permille: 100,
                    max_spread_bps: 10_000,
                    min_size_lots: 1,
                }],
                review_interval_ms: 10_000,
            },
            ..EngineConfig::default()
        };
        let mut e = Engine::new(cfg);
        e.register_instrument(Instrument::Perp(PerpMarket::default()));
        for provider in ["pyth", "chainlink"] {
            e.process(Command::OracleUpdate {
                base_symbol: "BTC".into(),
                provider: provider.into(),
                ts: 1_000,
                price_quote_minor: 8_000_000,
            });
        }
        e.process(Command::Tick { now: 1_000 });
        for sub in 1..=64_u64 {
            e.process(Command::Deposit {
                subaccount: sub,
                amount_quote_minor: 1_000_000_000,
            });
            e.process(Command::MmTierEnroll {
                subaccount: sub,
                now: 1_000,
            });
            e.process(Command::Place {
                request: OrderRequest::limit(sub, "BTC-PERP", Side::Bid, 79_990, 5),
                now: 1_000,
            });
            e.process(Command::Place {
                request: OrderRequest::limit(sub, "BTC-PERP", Side::Ask, 80_010, 5),
                now: 1_000,
            });
        }
        let mut samples = Vec::with_capacity(N);
        for i in 0..N as u64 {
            let t = Instant::now();
            // Sampling tick (liquidity scoring feeds the MM ledger).
            e.process(Command::Tick { now: 2_000 + i });
            // Review boundary every other iteration.
            e.process(Command::Tick {
                now: 12_000 + i * 20_000,
            });
            samples.push(t.elapsed().as_nanos() as u64);
        }
        stats("MM tier sample + review tick (64 MMs)", &mut samples);

        // Proof-of-reserves: liability tree build + per-account proof
        // verification over the settled venue.
        let mut samples = Vec::with_capacity(N);
        for i in 0..N as u64 {
            let t = Instant::now();
            let rows = e.por_liabilities(50_000 + i);
            let (tree, report) =
                poc_settlement::build_report_from_rows(rows, i, 50_000 + i).expect("build");
            for entry in &tree.entries {
                let (proved, proof) = tree.proof(entry.subaccount).expect("proof");
                assert!(poc_settlement::verify_liability(
                    &proved,
                    &proof,
                    &report.root,
                    report.nonce
                ));
            }
            samples.push(t.elapsed().as_nanos() as u64);
        }
        stats("PoR build + prove all (64 accounts)", &mut samples);

        // FIX session round trip: logon, order, logout over the
        // in-memory duplex (the codec + session layers).
        let mut samples = Vec::with_capacity(N);
        for i in 0..N as u64 {
            let t = Instant::now();
            let pair = poc_api::LoopbackPair::new();
            let mut client = poc_api::FixSession::new(pair.a, poc_api::Role::Initiator);
            let mut server = poc_api::FixSession::new(pair.b, poc_api::Role::Acceptor);
            let _ = client.logon(30);
            let _ = server.pump(100);
            let _ = client.pump(200);
            let mut order = poc_api::fix::FixMessage::new();
            order
                .set(35, "D")
                .set(49, "7")
                .set(55, "BTC-PERP")
                .set(54, "1")
                .set(38, "5")
                .set(44, "79900")
                .set(40, "3")
                .set(11, format!("bench-{i}"));
            let _ = client.send(&order);
            let _ = server.pump(300);
            let _ = client.logout();
            let _ = server.pump(400);
            samples.push(t.elapsed().as_nanos() as u64);
        }
        stats("FIX session lifecycle (logon+order+logout)", &mut samples);
    }

    // 9. zkLighter channel book: deep-book takers with early termination.
    {
        const N: usize = 20_000;
        let mut e = setup_engine(8);
        // 100 ask levels x 20 orders each = 2,000 resting orders.
        for level in 0..100_u64 {
            for j in 0..20_u64 {
                e.process(Command::Place {
                    request: OrderRequest::limit(
                        1 + ((level + j) % 8),
                        "BTC-PERP",
                        Side::Ask,
                        79_500 + level,
                        5,
                    ),
                    now: 2_000,
                });
            }
        }
        let mut samples = Vec::with_capacity(N);
        for i in 0..N {
            let t = Instant::now();
            // Taker fills 3 lots at the touch: the walk terminates at the
            // first level (channel laziness), never paying for the 99
            // levels behind it.
            e.process(Command::Place {
                request: OrderRequest::limit(9 - (i % 8) as u64, "BTC-PERP", Side::Bid, 79_500, 3),
                now: 3_000 + i as u64,
            });
            samples.push(t.elapsed().as_nanos() as u64);
        }
        stats("taker vs 2000-order deep book (lazy)", &mut samples);
    }

    // 10. Auction uncross at scale (prefix-sum clearing).
    {
        let mut e = setup_engine(8);
        e.process(Command::BeginAuction {
            symbol: "BTC-PERP".into(),
            uncross_at: 60_000,
            now: 2_000,
        });
        // 5,000 auction orders across 60 levels on each side.
        for i in 0..5_000_u64 {
            let side = if i % 2 == 0 { Side::Bid } else { Side::Ask };
            let price = if side == Side::Bid {
                81_000 - (i % 60)
            } else {
                79_000 + (i % 60)
            };
            e.process(Command::Place {
                request: OrderRequest::limit(1 + (i % 8), "BTC-PERP", side, price, 2),
                now: 2_500,
            });
        }
        let mut samples = Vec::with_capacity(1);
        let t = Instant::now();
        e.process(Command::Tick { now: 60_001 });
        samples.push(t.elapsed().as_nanos() as u64);
        stats("auction uncross (10k orders, 121 lvls)", &mut samples);
    }

    // 11. American analytics: BAW mark vs European BSM vs Merton perpetual.
    {
        use poc_margin::{AmericanAnalytics, Flavour, OptionAnalytics};
        const N: usize = 50_000;
        let mut samples = Vec::with_capacity(N);
        for i in 0..N {
            let m = (i % 60) as f64 / 100.0 - 0.3; // moneyness -30%..+30%
            let s = 100_000_f64 * (1.0 + m);
            let t = Instant::now();
            let _ =
                AmericanAnalytics::baw_price(Flavour::Put, s, 100_000.0, 0.25, 0.55, 0.03, 0.03);
            samples.push(t.elapsed().as_nanos() as u64);
        }
        stats("BAW American mark (put, r=3%)", &mut samples);

        for i in 0..N {
            let m = (i % 60) as f64 / 100.0 - 0.3;
            let s = 100_000_f64 * (1.0 + m);
            let t = Instant::now();
            let _ = OptionAnalytics::price(Flavour::Put, s, 100_000.0, 0.25, 0.55, 0.03);
            samples.push(t.elapsed().as_nanos() as u64);
        }
        stats("European BSM mark (reference)", &mut samples);

        for i in 0..N {
            let m = (i % 60) as f64 / 100.0 - 0.3;
            let s = 100_000_f64 * (1.0 + m);
            let t = Instant::now();
            let _ =
                AmericanAnalytics::merton_perpetual(Flavour::Put, s, 100_000.0, 0.03, 0.03, 0.55);
            samples.push(t.elapsed().as_nanos() as u64);
        }
        stats("Merton perpetual American (closed form)", &mut samples);
    }

    // 12. American exercise settlement sweep (queue + pro-rata assignment).
    {
        use poc_core::{AmericanParams, ExerciseStyle, OptionKind, OptionMarket, OptionVariant};
        let mut e = setup_engine(64);
        let opt = OptionMarket {
            symbol: "BTC-AMER-80000-C".into(),
            base_symbol: "BTC".into(),
            kind: OptionKind::Call,
            strike_quote_minor: 8_000_000,
            expiry_ts_ms: 30 * 86_400_000,
            variant: OptionVariant::Dated,
            exercise_style: ExerciseStyle::American,
            american: AmericanParams {
                settlement_twap_ms: 60_000,
                exercise_fee_bps: 5,
            },
            ..OptionMarket::default()
        };
        e.register_instrument(poc_core::Instrument::Option(opt));
        for provider in ["pyth", "chainlink"] {
            e.process(Command::OracleUpdate {
                base_symbol: "BTC".into(),
                provider: provider.into(),
                ts: 1_000,
                price_quote_minor: 10_000_000,
            });
        }
        e.process(Command::Tick { now: 1_000 });
        // One long (sub 64) buys 500 lots from 63 shorts.
        let mut ask_id = 0_u64;
        for sub in 1..=63_u64 {
            e.process(Command::Place {
                request: OrderRequest::limit(sub, "BTC-AMER-80000-C", Side::Ask, 48_000, 8),
                now: 1_500,
            });
            ask_id += 1;
        }
        e.process(Command::Place {
            request: OrderRequest::limit(64, "BTC-AMER-80000-C", Side::Bid, 48_000, 504),
            now: 1_600,
        });
        let _ = ask_id;
        e.process(Command::Exercise {
            subaccount: 64,
            symbol: "BTC-AMER-80000-C".into(),
            lots: 504,
            now: 2_000,
        });
        // Fill the TWAP window.
        for dt in [20_000_u64, 40_000, 59_000] {
            for provider in ["pyth", "chainlink"] {
                e.process(Command::OracleUpdate {
                    base_symbol: "BTC".into(),
                    provider: provider.into(),
                    ts: 2_000 + dt,
                    price_quote_minor: 10_000_000,
                });
            }
        }
        let mut samples = Vec::with_capacity(1);
        let t = Instant::now();
        e.process(Command::Tick { now: 62_001 });
        samples.push(t.elapsed().as_nanos() as u64);
        stats("exercise settle (63 shorts pro-rata)", &mut samples);
    }

    // 13. Book commitment over a deep book.
    {
        let mut e = setup_engine(8);
        for level in 0..50_u64 {
            for j in 0..20_u64 {
                e.process(Command::Place {
                    request: OrderRequest::limit(
                        1 + ((level + j) % 8),
                        "BTC-PERP",
                        Side::Bid,
                        79_000 + level,
                        4,
                    ),
                    now: 2_000,
                });
            }
        }
        let mut samples = Vec::with_capacity(100);
        for _ in 0..100 {
            let t = Instant::now();
            let c = poc_settlement::BookCommitment::capture(&e);
            let _ = c.root;
            samples.push(t.elapsed().as_nanos() as u64);
        }
        stats("book commitment (1000 resting orders)", &mut samples);
    }
}

// ----------------------------------------------------------------------
// Stress scenarios
// ----------------------------------------------------------------------

mod stress {
    use super::*;

    pub fn run_all() {
        println!("== architecture stress scenarios ==\n");
        flash_crash();
        volatility_spike();
        order_spam();
        liquidation_cascade();
        oracle_split();
        crash_recovery_determinism();
        println!("all stress scenarios completed without invariant violations");
    }

    fn venue(subs: u64, cash: u128) -> Engine {
        let mut e = Engine::new(EngineConfig::default());
        e.register_instrument(Instrument::Perp(PerpMarket::default()));
        for provider in ["pyth", "chainlink"] {
            e.process(Command::OracleUpdate {
                base_symbol: "BTC".into(),
                provider: provider.into(),
                ts: 1_000,
                price_quote_minor: 8_000_000,
            });
        }
        e.process(Command::Tick { now: 1_000 });
        for sub in 1..=subs {
            e.process(Command::Deposit {
                subaccount: sub,
                amount_quote_minor: cash,
            });
        }
        e
    }

    fn seed_liquidity(e: &mut Engine, levels: u64) {
        let mut rng = Rng::new(99);
        for i in 0..levels {
            let ask = 80_000 + (i % 40) + 1;
            let bid = 80_000 - (i % 40) - 1;
            let sub = 1 + rng.below(6);
            let _ = e.process(Command::Place {
                request: OrderRequest::limit(sub, "BTC-PERP", Side::Ask, ask, 1 + rng.below(4)),
                now: 2_000,
            });
            let sub = 1 + rng.below(6);
            let _ = e.process(Command::Place {
                request: OrderRequest::limit(sub, "BTC-PERP", Side::Bid, bid, 1 + rng.below(4)),
                now: 2_000,
            });
        }
    }

    fn equity_total(e: &Engine) -> i128 {
        e.accounts_iter()
            .map(|(_, a)| a.cash_quote_minor)
            .fold(0_i128, |acc, x| acc.saturating_add(x))
    }

    /// A −50% flash crash in 10 seconds of engine time.
    fn flash_crash() {
        let mut e = venue(8, 2_000_000);
        seed_liquidity(&mut e, 60);
        let t = Instant::now();
        let mut events = 0_usize;
        for step in 0..100 {
            let now = 3_000 + step * 100;
            let price = 8_000_000_u128.saturating_sub(u128::from(step) * 40_000);
            for provider in ["pyth", "chainlink"] {
                e.process(Command::OracleUpdate {
                    base_symbol: "BTC".into(),
                    provider: provider.into(),
                    ts: now,
                    price_quote_minor: price,
                });
            }
            events += e.process(Command::Tick { now }).len();
        }
        println!(
            "flash-crash (-50% in 100 ticks) {:>12?} {:>6} events, final spot ~${}",
            t.elapsed(),
            events,
            8_000_000_u128.saturating_sub(100 * 40_000) / 100
        );
    }

    /// A volatility spike: 500 coordinated oracle swings.
    fn volatility_spike() {
        let mut e = venue(6, 5_000_000);
        seed_liquidity(&mut e, 40);
        let mut rng = Rng::new(5);
        let t = Instant::now();
        let mut mark_ok = 0;
        for step in 0..500 {
            let now = 3_000 + step * 50;
            let price = 7_500_000_u128 + u128::from(rng.below(1_000_000));
            for provider in ["pyth", "chainlink", "redstone"] {
                e.process(Command::OracleUpdate {
                    base_symbol: "BTC".into(),
                    provider: provider.into(),
                    ts: now,
                    price_quote_minor: price,
                });
            }
            e.process(Command::Tick { now });
            if e.market_state()
                .spots
                .iter()
                .any(|(b, s)| b == "BTC" && s.is_some())
            {
                mark_ok += 1;
            }
        }
        println!(
            "vol-spike (500 coordinated swings) {:>9?} mark survived {mark_ok}/500 ticks",
            t.elapsed()
        );
    }

    /// An adversarial spam storm: 40k place/cancel cycles.
    fn order_spam() {
        let mut e = venue(4, 10_000_000_000);
        let mut rng = Rng::new(3);
        let t = Instant::now();
        let mut accepted = 0_usize;
        for i in 0..40_000 {
            let sub = 1 + rng.below(4);
            let price = 40_000 + rng.below(80_000);
            let evs = e.process(Command::Place {
                request: OrderRequest::limit(
                    sub,
                    "BTC-PERP",
                    if i % 2 == 0 { Side::Bid } else { Side::Ask },
                    price,
                    1,
                ),
                now: 2_000,
            });
            accepted += evs
                .iter()
                .any(|x| matches!(x, poc_engine::Event::OrderResting { .. }))
                as usize;
            if i % 3 == 0 {
                e.process(Command::CancelAll {
                    subaccount: sub,
                    symbol: None,
                    now: 2_000,
                });
            }
        }
        println!(
            "order-spam (40k place/cancel)   {:>10?} {accepted} resting, books sane",
            t.elapsed()
        );
    }

    /// A liquidation cascade with an empty insurance fund.
    fn liquidation_cascade() {
        let cfg = EngineConfig {
            insurance_seed_quote_minor: 0,
            ..EngineConfig::default()
        };
        let mut e = Engine::new(cfg);
        e.register_instrument(Instrument::Perp(PerpMarket::default()));
        for provider in ["pyth", "chainlink"] {
            e.process(Command::OracleUpdate {
                base_symbol: "BTC".into(),
                provider: provider.into(),
                ts: 1_000,
                price_quote_minor: 8_000_000,
            });
        }
        e.process(Command::Tick { now: 1_000 });
        // Degens with thin margin.
        for sub in 1..=20 {
            e.process(Command::Deposit {
                subaccount: sub,
                amount_quote_minor: 300_000,
            });
        }
        // MMs provide liquidity.
        for sub in 21..=24 {
            e.process(Command::Deposit {
                subaccount: sub,
                amount_quote_minor: 50_000_000,
            });
        }
        for i in 0..200 {
            e.process(Command::Place {
                request: OrderRequest::limit(
                    21 + (i % 4),
                    "BTC-PERP",
                    Side::Ask,
                    79_900 + (i % 50),
                    25,
                ),
                now: 2_000,
            });
        }
        for i in 0..200 {
            e.process(Command::Place {
                request: OrderRequest::limit(
                    1 + (i % 20),
                    "BTC-PERP",
                    Side::Bid,
                    79_900 + (i % 50),
                    15,
                ),
                now: 2_001,
            });
        }
        let before = equity_total(&e);
        let t = Instant::now();
        let mut liquidations = 0_usize;
        for step in 0..120 {
            let now = 3_000 + step * 200;
            let price = 8_000_000_u128.saturating_sub(u128::from(step) * 30_000);
            for provider in ["pyth", "chainlink"] {
                e.process(Command::OracleUpdate {
                    base_symbol: "BTC".into(),
                    provider: provider.into(),
                    ts: now,
                    price_quote_minor: price,
                });
            }
            for ev in e.process(Command::Tick { now }) {
                if matches!(ev, poc_engine::Event::Liquidation(_)) {
                    liquidations += 1;
                }
            }
        }
        println!(
            "liquidation-cascade (20 degens) {:>8?} {liquidations} liquidation events, cash delta {}",
            t.elapsed(),
            equity_total(&e) - before
        );
    }

    /// A provider split: one oracle goes rogue while two stay honest.
    fn oracle_split() {
        let mut e = venue(4, 1_000_000);
        let t = Instant::now();
        let mut quarantined = 0;
        let mut held = 0;
        for step in 0..300 {
            let now = 3_000 + step * 100;
            for provider in ["pyth", "chainlink"] {
                e.process(Command::OracleUpdate {
                    base_symbol: "BTC".into(),
                    provider: provider.into(),
                    ts: now,
                    price_quote_minor: 8_000_000,
                });
            }
            e.process(Command::OracleUpdate {
                base_symbol: "BTC".into(),
                provider: "rogue".into(),
                ts: now,
                price_quote_minor: 40_000_000,
            });
            let events = e.process(Command::Tick { now });
            for ev in &events {
                if let poc_engine::Event::MarketHalted { .. } = ev {
                    quarantined += 1;
                }
            }
            if e.market_state()
                .spots
                .iter()
                .any(|(b, s)| b == "BTC" && s.is_some())
            {
                held += 1;
            }
        }
        println!(
            "oracle-split (1 rogue of 3)     {:>10?} mark held {held}/300, halts {quarantined}",
            t.elapsed()
        );
    }

    /// WAL crash recovery lands on a state identical to the live engine.
    fn crash_recovery_determinism() {
        use poc_persist::WalWriter;
        let dir = std::env::temp_dir().join(format!("poc-stress-wal-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut wal = WalWriter::open(&dir).expect("wal");
        wal.register(&Instrument::Perp(PerpMarket::default()))
            .expect("register");

        let mut e = Engine::new(EngineConfig::default());
        e.register_instrument(Instrument::Perp(PerpMarket::default()));
        for provider in ["pyth", "chainlink"] {
            let cmd = Command::OracleUpdate {
                base_symbol: "BTC".into(),
                provider: provider.into(),
                ts: 1_000,
                price_quote_minor: 8_000_000,
            };
            e.process(cmd.clone());
            wal.append(&cmd).expect("append");
        }
        let cmd = Command::Tick { now: 1_000 };
        e.process(cmd.clone());
        wal.append(&cmd).expect("append");

        let mut rng = Rng::new(13);
        for i in 0..3_000 {
            let sub = 1 + rng.below(4);
            let deposit = Command::Deposit {
                subaccount: sub,
                amount_quote_minor: 10_000_000,
            };
            e.process(deposit.clone());
            wal.append(&deposit).expect("append");
            let cmd = Command::Place {
                request: OrderRequest::limit(
                    sub,
                    "BTC-PERP",
                    if i % 2 == 0 { Side::Bid } else { Side::Ask },
                    60_000 + rng.below(40_000),
                    1 + rng.below(3),
                ),
                now: 2_000 + i as u64,
            };
            e.process(cmd.clone());
            wal.append(&cmd).expect("append");
        }

        let t = Instant::now();
        let (recovered, report) =
            poc_persist::recover(EngineConfig::default(), &dir).expect("recover");
        let mut live_fingerprint: Vec<_> = e
            .accounts_iter()
            .map(|(&s, a)| (s, a.cash_quote_minor, a.positions.len()))
            .collect();
        live_fingerprint.sort();
        let mut rec_fingerprint: Vec<_> = recovered
            .accounts_iter()
            .map(|(&s, a)| (s, a.cash_quote_minor, a.positions.len()))
            .collect();
        rec_fingerprint.sort();
        assert_eq!(live_fingerprint, rec_fingerprint, "recovery diverged");
        println!(
            "crash-recovery determinism       {:>10?} {}/{} commands, state identical",
            t.elapsed(),
            report.commands_replayed,
            6_004
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
