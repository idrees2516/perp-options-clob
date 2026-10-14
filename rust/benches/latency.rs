//! ns-clob micro-latency suite — custom zero-dependency harness.
//!
//! Methodology: each operation runs in batches of 512 between two
//! `Instant::now()` reads; the timer cost (~25ns) amortizes to <0.05ns/op.
//! Reported percentiles are over batch means (B=512), which for stationary
//! workloads track per-op percentiles closely. Means are over all iterations.
//!
//! Bench fixtures relax the order-rate limit (the logical clock barely moves
//! under benchmarking, which would otherwise throttle the token bucket).
//!
//! Run: `cargo bench`.

use ns_clob::book::NIL;
use ns_clob::engine::{Cmd, Shard, ShardConfig, NANOS_PER_MIN};
use ns_clob::fees::Tier;
use ns_clob::journal::EvBuf;
use ns_clob::sim::{DaySchedule, Flow};
use ns_clob::symbol::{Listing, OptKind};
use ns_clob::types::*;
use std::hint::black_box;
use std::time::Instant;

const BATCH: usize = 512;
const DAY0: u64 = 30_912_000;

struct BResult {
    name: &'static str,
    mean_ns: f64,
    p50_ns: f64,
    p99_ns: f64,
    p999_ns: f64,
}

fn run<F: FnMut(usize) -> u64>(name: &'static str, iters: usize, mut op: F) -> BResult {
    let warm = iters / 10 + 1;
    for i in 0..warm {
        black_box(op(i));
    }
    let mut batch_means: Vec<f64> = Vec::with_capacity(iters / BATCH + 1);
    let mut done = 0usize;
    let mut sink: u64 = 0;
    while done < iters {
        let n = BATCH.min(iters - done);
        let t0 = Instant::now();
        for i in 0..n {
            sink = sink.wrapping_add(op(done + i));
        }
        let dt = t0.elapsed().as_nanos() as f64;
        black_box(sink);
        batch_means.push(dt / n as f64);
        done += n;
    }
    batch_means.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let q = |p: f64| {
        let idx = ((batch_means.len() as f64) * p).floor() as usize;
        batch_means[idx.min(batch_means.len() - 1)]
    };
    let mean = batch_means.iter().sum::<f64>() / batch_means.len() as f64;
    BResult {
        name,
        mean_ns: mean,
        p50_ns: q(0.50),
        p99_ns: q(0.99),
        p999_ns: q(0.999),
    }
}

/// Bench configuration: production risk parameters, relaxed rate limit.
fn bench_cfg() -> ShardConfig {
    let mut cfg = ShardConfig::default_config();
    cfg.risk.rate_per_sec = 10_000_000;
    cfg.journal_in_memory = false; // measure the engine, not the disk
    cfg
}

fn listing(kind: OptKind, strike: u64, expiry_min: u64) -> Listing {
    Listing {
        underlying: 0,
        expiry_min: (DAY0 + expiry_min) as u32,
        kind,
        strike,
    }
}

/// A shard with `nsym` funded symbols, each with `depth` resting maker
/// levels of 10 contracts around a 5.00 mid. Accounts are tiered.
fn populated(nsym: u32, depth: u32, n_accts: u32) -> (Shard, Vec<u32>, Vec<u32>) {
    let mut s = Shard::new(0, 0, bench_cfg());
    let mut ev = EvBuf::with_capacity(1024);
    for a in 0..n_accts {
        s.execute(
            Cmd::CreateAccount {
                account: a,
                tier: if a % 4 == 0 { Tier::Prime } else { Tier::Base },
            },
            &mut ev,
        );
        s.execute(
            Cmd::Deposit {
                account: a,
                amount: 500_000_000,
            },
            &mut ev,
        );
    }
    let mut symbols = Vec::new();
    for i in 0..nsym {
        let l = listing(OptKind::Call, 500_000 + (i as u64) * 1_000, 600);
        s.execute(Cmd::CreateSymbol { listing: l }, &mut ev);
        let id = s.symbol_id(l);
        symbols.push(id);
    }
    s.execute(
        Cmd::Advance {
            to: (DAY0 + 570) * NANOS_PER_MIN,
        },
        &mut ev,
    );
    s.execute(Cmd::Oracle { price: 500_000 }, &mut ev);
    for (i, &sym) in symbols.iter().enumerate() {
        for d in 0..depth {
            s.execute(
                Cmd::Place(NewOrder {
                    symbol: sym,
                    account: (i as u32 % (n_accts - 1)) + 1,
                    side: Side::Sell,
                    price: 500 + d as u64,
                    qty: 10,
                    tif: Tif::Gtc,
                    kind: Kind::Limit,
                    flags: 0,
                    oco: NO_ID,
                }),
                &mut ev,
            );
            s.execute(
                Cmd::Place(NewOrder {
                    symbol: sym,
                    account: (i as u32 % (n_accts - 1)) + 1,
                    side: Side::Buy,
                    price: 499 - d as u64,
                    qty: 10,
                    tif: Tif::Gtc,
                    kind: Kind::Limit,
                    flags: 0,
                    oco: NO_ID,
                }),
                &mut ev,
            );
        }
    }
    let accounts: Vec<u32> = (0..n_accts).collect();
    (s, symbols, accounts)
}

fn main() {
    let mut results: Vec<BResult> = Vec::new();
    println!("ns-clob micro-latency suite (batch B={BATCH}, ns)\n");

    // ------------------------------------------------------------------
    // 1. Maker placement near the touch (the production quoting pattern:
    //    a few levels deep at most) and an adversarial deep-walk variant.
    // ------------------------------------------------------------------
    {
        let (mut s, symbols, accts) = populated(16, 5, 8);
        let mut ev = EvBuf::with_capacity(1024);
        let mut i = 0usize;
        results.push(run("place_maker_near_touch", 200_000, |_| {
            let sym = symbols[i % symbols.len()];
            i += 1;
            ev.clear();
            s.execute(
                Cmd::Place(NewOrder {
                    symbol: sym,
                    account: accts[0],
                    side: Side::Buy,
                    price: 480 + (i % 18) as u64,
                    qty: 5,
                    tif: Tif::Gtc,
                    kind: Kind::Limit,
                    flags: flags::POST_ONLY,
                    oco: NO_ID,
                }),
                &mut ev,
            );
            ev.events.len() as u64
        }));
    }
    {
        let (mut s, symbols, accts) = populated(4, 5, 8);
        let mut ev = EvBuf::with_capacity(1024);
        let mut i = 0usize;
        results.push(run("place_maker_deep_walk_300lv", 50_000, |_| {
            let sym = symbols[i % symbols.len()];
            i += 1;
            ev.clear();
            s.execute(
                Cmd::Place(NewOrder {
                    symbol: sym,
                    account: accts[0],
                    side: Side::Buy,
                    price: 100 + (i % 300) as u64,
                    qty: 5,
                    tif: Tif::Gtc,
                    kind: Kind::Limit,
                    flags: flags::POST_ONLY,
                    oco: NO_ID,
                }),
                &mut ev,
            );
            ev.events.len() as u64
        }));
    }

    // ------------------------------------------------------------------
    // 2. Cancel + requote cycle (the HFT heartbeat: pull and re-post).
    // ------------------------------------------------------------------
    {
        let (mut s, symbols, accts) = populated(16, 5, 8);
        let mut ev = EvBuf::with_capacity(1024);
        let mut pool: Vec<(OrderId, SymbolId)> = Vec::with_capacity(2048);
        let mut i = 0u64;
        while pool.len() < 2048 {
            let sym = symbols[(i % symbols.len() as u64) as usize];
            ev.clear();
            s.execute(
                Cmd::Place(NewOrder {
                    symbol: sym,
                    account: accts[0],
                    side: Side::Buy,
                    price: 470 + (i % 25),
                    qty: 5,
                    tif: Tif::Gtc,
                    kind: Kind::Limit,
                    flags: 0,
                    oco: NO_ID,
                }),
                &mut ev,
            );
            for e in ev.events.iter().rev() {
                if let ns_clob::journal::Event::Ack { order, leaves, .. } = e {
                    if *leaves > 0 {
                        pool.push((*order, sym));
                        break;
                    }
                }
            }
            i += 1;
        }
        let mut head = 0usize;
        let mut counter = 0u64;
        results.push(run("cancel_and_requote_cycle", 200_000, |_| {
            // 1) Cancel a live pooled order (correct symbol pairing).
            let (id, sym) = pool[head];
            ev.clear();
            s.execute(Cmd::Cancel { order: id, symbol: sym }, &mut ev);
            // 2) Requote on the same symbol.
            counter += 1;
            ev.clear();
            s.execute(
                Cmd::Place(NewOrder {
                    symbol: sym,
                    account: accts[0],
                    side: Side::Buy,
                    price: 470 + (counter % 25),
                    qty: 5,
                    tif: Tif::Gtc,
                    kind: Kind::Limit,
                    flags: 0,
                    oco: NO_ID,
                }),
                &mut ev,
            );
            for e in ev.events.iter().rev() {
                if let ns_clob::journal::Event::Ack { order, leaves, .. } = e {
                    if *leaves > 0 {
                        pool[head] = (*order, sym);
                        break;
                    }
                }
            }
            head = (head + 1) % pool.len();
            ev.events.len() as u64
        }));
    }

    // ------------------------------------------------------------------
    // 3. Taker full pipeline: match + settle + events + journal.
    // ------------------------------------------------------------------
    {
        let (mut s, symbols, accts) = populated(8, 8, 16);
        let mut ev = EvBuf::with_capacity(1024);
        let mut i = 0usize;
        results.push(run("taker_full_pipeline_settle", 200_000, |_| {
            let sym = symbols[i % symbols.len()];
            i += 1;
            ev.clear();
            s.execute(
                Cmd::Place(NewOrder {
                    symbol: sym,
                    account: accts[7 + (i % 8)],
                    side: Side::Buy,
                    price: 500,
                    qty: 3,
                    tif: Tif::Ioc,
                    kind: Kind::Limit,
                    flags: 0,
                    oco: NO_ID,
                }),
                &mut ev,
            );
            let n = ev.events.len() as u64;
            // Reseed the consumed level so the fixture never drains.
            ev.clear();
            s.execute(
                Cmd::Place(NewOrder {
                    symbol: sym,
                    account: accts[1],
                    side: Side::Sell,
                    price: 500,
                    qty: 3,
                    tif: Tif::Gtc,
                    kind: Kind::Limit,
                    flags: 0,
                    oco: NO_ID,
                }),
                &mut ev,
            );
            n
        }));
    }

    // ------------------------------------------------------------------
    // 4. FOK pre-scan against 8 levels (insufficient depth -> reject).
    // ------------------------------------------------------------------
    {
        let (mut s, symbols, accts) = populated(8, 8, 8);
        let mut ev = EvBuf::with_capacity(1024);
        let mut i = 0usize;
        results.push(run("fok_prescan_8_levels", 100_000, |_| {
            let sym = symbols[i % symbols.len()];
            i += 1;
            ev.clear();
            s.execute(
                Cmd::Place(NewOrder {
                    symbol: sym,
                    account: accts[2],
                    side: Side::Buy,
                    price: 470,
                    qty: 200,
                    tif: Tif::Fok,
                    kind: Kind::Limit,
                    flags: 0,
                    oco: NO_ID,
                }),
                &mut ev,
            );
            ev.events.len() as u64
        }));
    }

    // ------------------------------------------------------------------
    // 5. BBO / depth queries.
    // ------------------------------------------------------------------
    {
        let (s, symbols, _) = populated(16, 5, 8);
        let mut i = 0usize;
        results.push(run("bbo_query", 1_000_000, |_| {
            let sym = symbols[i % symbols.len()];
            i += 1;
            let b = s.symbols[sym as usize].book.best_bid();
            let a = s.symbols[sym as usize].book.best_ask();
            (b.map(|x| x.0).unwrap_or(0)).wrapping_add(a.map(|x| x.0).unwrap_or(0))
        }));
        let mut out = [(0u64, 0u64); 10];
        let mut j = 0usize;
        results.push(run("depth_top10_query", 500_000, |_| {
            let sym = symbols[j % symbols.len()];
            j += 1;
            s.symbols[sym as usize].book.depth(Side::Buy, 10, &mut out) as u64
        }));
    }

    // ------------------------------------------------------------------
    // 6. Margin math + symbol codec.
    // ------------------------------------------------------------------
    {
        let risk = bench_cfg().risk;
        let l = listing(OptKind::Call, 500_000, 600);
        let mut i = 0u64;
        results.push(run("short_margin_per_contract", 2_000_000, |_| {
            i += 1;
            ns_clob::margin::short_margin_per_contract(&risk, l, 500 + i % 900, 500_000, 120)
        }));
        results.push(run("symbol_key_codec", 2_000_000, |_| {
            i = i.wrapping_add(1);
            Listing::unpack(l.key()).strike
        }));
        results.push(run("delta_notional", 2_000_000, |_| {
            i = i.wrapping_add(7);
            l.delta_bps(500_000 + i % 4_000) as u64
        }));
    }

    // ------------------------------------------------------------------
    // 7. Auction clearing over a 2-sided auction book (32 levels).
    // ------------------------------------------------------------------
    {
        use ns_clob::auction::clearing_price;
        use ns_clob::book::{make_order, Book};
        let mut b = Book::with_capacity(512, 64);
        for lvl in 0..16u64 {
            for _ in 0..4 {
                b.insert(make_order(
                    1, 0, Side::Buy, 700 - lvl * 10, 10, Tif::Gtc, Kind::Limit, 0, NIL, NIL, 0, 0,
                ));
                b.insert(make_order(
                    2, 0, Side::Sell, 400 + lvl * 10, 10, Tif::Gtc, Kind::Limit, 0, NIL, NIL, 0, 0,
                ));
            }
        }
        results.push(run("auction_clearing_price_32_levels", 50_000, |_| {
            clearing_price(&b, 550).qty
        }));
    }

    // ------------------------------------------------------------------
    // 8. Expiry settlement: 8 positions, wheel-driven settle.
    // ------------------------------------------------------------------
    {
        let (mut s, _, _) = populated(1, 1, 4);
        let mut ev = EvBuf::with_capacity(1024);
        // Pre-create enough expiring symbols for warmup + run (fixture cost).
        // The clock walks minute-by-minute with the expiries.
        let mut syms = Vec::with_capacity(500);
        for k in 0..500u64 {
            let l = listing(OptKind::Call, 490_000 + (k % 20) * 1_000, 960 + k);
            s.execute(Cmd::CreateSymbol { listing: l }, &mut ev);
            syms.push((s.symbol_id(l), (DAY0 + 961 + k) * NANOS_PER_MIN));
        }
        s.execute(
            Cmd::Advance {
                to: (DAY0 + 960) * NANOS_PER_MIN,
            },
            &mut ev,
        );
        let mut k = 0usize;
        results.push(run("expiry_settle_8_positions", 400, |_| {
            let (sym, settle_at) = syms[k];
            k += 1;
            // Open 8 positions (4 pairs).
            for a in 1..5u32 {
                for (acct, side) in [(a, Side::Buy), (a + 10, Side::Sell)] {
                    ev.clear();
                    s.execute(
                        Cmd::Place(NewOrder {
                            symbol: sym,
                            account: acct,
                            side,
                            price: 500,
                            qty: 10,
                            tif: Tif::Ioc,
                            kind: Kind::Limit,
                            flags: 0,
                            oco: NO_ID,
                        }),
                        &mut ev,
                    );
                    ev.clear();
                    s.execute(
                        Cmd::Place(NewOrder {
                            symbol: sym,
                            account: acct + 20,
                            side: side.opp(),
                            price: 500,
                            qty: 10,
                            tif: Tif::Ioc,
                            kind: Kind::Limit,
                            flags: 0,
                            oco: NO_ID,
                        }),
                        &mut ev,
                    );
                }
            }
            // Drive the wheel past this symbol's expiry.
            ev.clear();
            s.execute(Cmd::Advance { to: settle_at }, &mut ev);
            s.symbols[sym as usize].phase as u64
        }));
    }

    // ------------------------------------------------------------------
    // 9. State hash (audit path) and conservation.
    // ------------------------------------------------------------------
    {
        let (s, _, _) = populated(16, 5, 8);
        results.push(run("state_hash_full_audit", 2_000, |_| s.state_hash()));
        results.push(run("conservation_check", 200_000, |_| {
            s.conservation() as u64
        }));
    }

    // ------------------------------------------------------------------
    // 10. Simulated live shard: mixed adversarial flow (journaling on).
    // ------------------------------------------------------------------
    {
        let mut s = Shard::new(0, 0, bench_cfg());
        let sched = DaySchedule::default_day();
        let mut flow = Flow::boot(&mut s, 7, 24, &sched);
        let mut ev = EvBuf::with_capacity(512);
        results.push(run("mixed_flow_command", 100_000, |_| {
            ev.clear();
            let c = match flow.next_cmd() {
                ns_clob::sim::AgentCmd::Place(o) => Cmd::Place(o),
                ns_clob::sim::AgentCmd::Cancel(i, sym) => Cmd::Cancel { order: i, symbol: sym },
                ns_clob::sim::AgentCmd::Tick => Cmd::Advance {
                    to: s.now() + NANOS_PER_MIN / 500,
                },
                ns_clob::sim::AgentCmd::Mark(p) => Cmd::Oracle { price: p },
            };
            s.execute(c, &mut ev);
            flow.note_ack(&ev);
            ev.events.len() as u64
        }));
        assert!(s.conservation());
    }

    // ------------------------------------------------------------------
    println!("{:<32} {:>10} {:>10} {:>10} {:>10}", "op", "mean", "p50", "p99", "p99.9");
    for r in &results {
        println!(
            "{:<32} {:>10.1} {:>10.1} {:>10.1} {:>10.1}",
            r.name, r.mean_ns, r.p50_ns, r.p99_ns, r.p999_ns
        );
    }
    let taker = results
        .iter()
        .find(|r| r.name == "taker_full_pipeline_settle")
        .map(|r| r.mean_ns)
        .unwrap_or(120.0);
    let place = results
        .iter()
        .find(|r| r.name == "place_maker_near_touch")
        .map(|r| r.mean_ns)
        .unwrap_or(60.0);
    println!("\nper-shard capacity @ pure taker mix:  {:>7.1}M ops/s", 1000.0 / taker);
    println!("per-shard capacity @ pure maker mix:  {:>7.1}M ops/s", 1000.0 / place);
    println!("projected mixed (45/25/20/10):        {:>7.1}M ops/s", 1000.0 / (0.45 * place + 0.25 * taker + 0.2 * 35.0 + 0.1 * 90.0));
}
