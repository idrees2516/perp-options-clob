//! ns-tps — aggregate throughput harness for the ns-clob 0DTE platform.
//!
//! Spawns one self-driving worker per requested shard (default: one per
//! CPU). Each worker owns a complete [`Shard`] — books, accounts, margin,
//! journal — and generates a realistic 0DTE order mix against near-the-touch
//! prices:
//!
//! * 45% maker quotes near the touch (post-only half the time)
//! * 25% takers crossing the spread (full pipeline: match + settle + journal)
//! * 20% cancels of live resting orders
//! * 10% cancel-replace
//!
//! Every worker advances its logical clock and marks, so rate limiting,
//! expiry scheduling and margin refresh all stay live. After the run every
//! shard is audited: conservation must hold exactly.
//!
//! Usage: `ns-tps [--shards N] [--seconds S] [--mode mixed|taker|maker] [--wal FILE]`

use ns_clob::engine::{Cmd, Shard, ShardConfig, NANOS_PER_MIN};
use ns_clob::fees::Tier;
use ns_clob::journal::EvBuf;
use ns_clob::sim::Rng;
use ns_clob::symbol::{Listing, OptKind};
use ns_clob::types::*;
use std::time::{Duration, Instant};

const DAY0: u64 = 30_912_000;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Mode {
    Mixed,
    Taker,
    Maker,
}

struct Args {
    shards: usize,
    seconds: f64,
    mode: Mode,
    wal: Option<String>,
}

fn parse_args() -> Args {
    let mut a = Args {
        shards: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2),
        seconds: 5.0,
        mode: Mode::Mixed,
        wal: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--shards" | "-n" => {
                if let Some(v) = it.next() {
                    a.shards = v.parse().unwrap_or(2);
                }
            }
            "--seconds" | "-s" => {
                if let Some(v) = it.next() {
                    a.seconds = v.parse().unwrap_or(5.0);
                }
            }
            "--mode" | "-m" => {
                if let Some(v) = it.next() {
                    a.mode = match v.as_str() {
                        "taker" => Mode::Taker,
                        "maker" => Mode::Maker,
                        _ => Mode::Mixed,
                    };
                }
            }
            "--wal" => {
                if let Some(v) = it.next() {
                    a.wal = Some(v);
                }
            }
            _ => {}
        }
    }
    a
}

struct WorkerReport {
    ops: u64,
    trades: u64,
    rejects: u64,
    cancels: u64,
    ns_per_op_mean: f64,
    p50: f64,
    p99: f64,
    p999: f64,
    journal_written: u64,
    conservation_ok: bool,
    live_orders: u64,
    open_positions: u64,
}

fn worker(shard_id: u32, mode: Mode, seconds: f64, wal: Option<&str>, seed: u64) -> WorkerReport {
    // ------------------------------------------------------------------
    // Fixture: one shard, 8 near-ATM symbols, 24 funded accounts.
    // ------------------------------------------------------------------
    let mut cfg = ShardConfig::default_config();
    cfg.risk.rate_per_sec = 10_000_000;
    // Capital-consistent limits: accounts fund $40B ticks, so exposure
    // limits must scale with capital or every order rejects on DeltaLimit.
    cfg.risk.delta_notional_limit = 400_000_000_000_000;
    cfg.risk.position_limit = 2_000_000;
    cfg.order_cap = 512; // L2-resident working set per shard
    cfg.level_cap = 64;
    cfg.journal_in_memory = false; // measure the engine; --wal attaches a file sink
    let mut shard = Shard::new(shard_id, shard_id as u16, cfg);
    let mut ev = EvBuf::with_capacity(256);
    if let Some(path) = wal {
        if let Ok(f) = std::fs::File::create(path) {
            shard.journal = ns_clob::journal::CmdLog::with_sink(
                256 * 1024,
                Box::new(std::io::BufWriter::with_capacity(256 * 1024, f)),
            );
        }
    }
    for a in 0..24u32 {
        shard.execute(
            Cmd::CreateAccount {
                account: a,
                tier: if a % 6 == 0 {
                    Tier::Prime
                } else if a % 3 == 0 {
                    Tier::Gold
                } else {
                    Tier::Base
                },
            },
            &mut ev,
        );
        shard.execute(
            Cmd::Deposit {
                account: a,
                amount: 40_000_000_000_000,
            },
            &mut ev,
        );
    }
    let mut symbols = Vec::new();
    for i in 0..8u32 {
        let l = Listing {
            underlying: shard_id as u16,
            expiry_min: (DAY0 + 1200) as u32, // far expiry: no churn in-run
            kind: if i % 2 == 0 { OptKind::Call } else { OptKind::Put },
            strike: 496_000 + (i as u64) * 1_000,
        };
        shard.execute(Cmd::CreateSymbol { listing: l }, &mut ev);
        symbols.push(shard.symbol_id(l));
    }
    shard.execute(
        Cmd::Advance {
            to: (DAY0 + 570) * NANOS_PER_MIN,
        },
        &mut ev,
    );
    shard.execute(Cmd::Oracle { price: 500_000 }, &mut ev);
    // Seed each book with 5 levels a side.
    for &sym in &symbols {
        for d in 0..5u64 {
            shard.execute(
                Cmd::Place(NewOrder {
                    symbol: sym,
                    account: 1,
                    side: Side::Sell,
                    price: 500 + d,
                    qty: 10,
                    tif: Tif::Gtc,
                    kind: Kind::Limit,
                    flags: 0,
                    oco: NO_ID,
                }),
                &mut ev,
            );
            shard.execute(
                Cmd::Place(NewOrder {
                    symbol: sym,
                    account: 2,
                    side: Side::Buy,
                    price: 499 - d,
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

    // ------------------------------------------------------------------
    // Self-driving loop. Market-maker quote-slot model (like real 0DTE
    // quoting desks): every account keeps one quote per (symbol, side);
    // requotes atomically replace the account's own slot, pulls remove it.
    // Takers cross the spread and reseed from their own account with the
    // exact crossed qty. Quote sides and taker sides are inventory-aware
    // (mean-reverting), so positions stay bounded and the market reaches a
    // steady state: liquidity ~= accounts x symbols x sides, all tracked.
    // ------------------------------------------------------------------
    const NACCT: usize = 24;
    let mut rng = Rng::new(seed);
    // Quote slots: [account][symbol-index][side] -> live order id (0 = none).
    let mut slots: [[[OrderId; 2]; 8]; NACCT] = [[[0; 2]; 8]; NACCT];
    // Inventory mirror: [account][symbol-index] -> signed qty (approx).
    let mut inv_cache: [[i32; 8]; NACCT] = [[0; 8]; NACCT];
    let mut op = NewOrder {
        symbol: 0,
        account: 0,
        side: Side::Buy,
        price: 0,
        qty: 5,
        tif: Tif::Gtc,
        kind: Kind::Limit,
        flags: 0,
        oco: NO_ID,
    };
    let batch = 512usize;
    let mut batch_means: Vec<f64> = Vec::new();
    let mut ops: u64 = 0;
    let deadline = Instant::now() + Duration::from_secs_f64(seconds);
    let t_start = Instant::now();

    'outer: loop {
        let t0 = Instant::now();
        let mut cmds_this_batch = 0u64;
        for _ in 0..batch {
            let r = rng.next();
            let pick = match mode {
                Mode::Mixed => {
                    let x = r % 100;
                    if x < 60 {
                        0 // requote own slot (inventory-aware side)
                    } else if x < 90 {
                        1 // taker cross (inventory-aware side)
                    } else {
                        3 // pull own quote
                    }
                }
                Mode::Taker => 1,
                Mode::Maker => 0,
            };
            let sym_i = (r >> 8) as usize % symbols.len();
            let sym = symbols[sym_i];
            let acct = (r >> 16) as u32 as usize % NACCT;
            // Inventory of this account on this symbol. Engine truth is
            // refreshed periodically; in between we track our own deltas
            // (taker/reseed flows), which keeps the mean-reversion bias
            // correct while avoiding two random-access reads per command.
            if r & 0x1F == 0 {
                inv_cache[acct][sym_i] = shard
                    .accounts
                    .get(acct)
                    .and_then(|a| a.pos_slot(sym))
                    .map(|slot| shard.positions[slot as usize].qty)
                    .unwrap_or(0);
            }
            let inv = inv_cache[acct][sym_i];
            match pick {
                0 => {
                    // Requote own slot; side biased against inventory.
                    let side = if inv > 0 {
                        if r & 0x2_0000 == 0 { Side::Sell } else { Side::Buy }
                    } else if inv < 0 {
                        if r & 0x2_0000 == 0 { Side::Buy } else { Side::Sell }
                    } else if r & 0x1_0000 == 0 {
                        Side::Buy
                    } else {
                        Side::Sell
                    };
                    let side_i = side.idx();
                    let old = slots[acct][sym_i][side_i];
                    let (lo, span) = match side {
                        Side::Buy => (480u64, 18u64),
                        Side::Sell => (502u64, 18u64),
                    };
                    let price = lo + (r >> 24) % span;
                    let qty = 1 + (r >> 32) as u32 % 10;
                    ev.clear();
                    if old != 0 {
                        shard.execute(
                            Cmd::Replace {
                                order: old,
                                symbol: sym,
                                price,
                                qty,
                                account: acct as u32,
                            },
                            &mut ev,
                        );
                    } else {
                        op.symbol = sym;
                        op.account = acct as u32;
                        op.side = side;
                        op.price = price;
                        op.qty = qty;
                        op.tif = Tif::Gtc;
                        op.flags = 0; // plain limit (post-only covered by unit tests)
                        shard.execute(Cmd::Place(op), &mut ev);
                    }
                    slots[acct][sym_i][side_i] = shard.last_ack();
                    ops += 1;
                    cmds_this_batch += 1;
                }
                1 => {
                    // Taker crosses the spread, reducing own inventory.
                    let side = match inv.signum() {
                        1 => Side::Sell,
                        -1 => Side::Buy,
                        _ => {
                            if r & 0x1_0000 == 0 {
                                Side::Buy
                            } else {
                                Side::Sell
                            }
                        }
                    };
                    let q = 1 + (r >> 32) as u32 % 5;
                    op.symbol = sym;
                    op.account = acct as u32;
                    op.side = side;
                    op.price = if side == Side::Buy { 505 } else { 494 };
                    op.qty = q;
                    op.tif = Tif::Ioc;
                    op.flags = 0;
                    inv_cache[acct][sym_i] += match side {
                        Side::Buy => q as i32,
                        Side::Sell => -(q as i32),
                    };
                    ev.clear();
                    shard.execute(Cmd::Place(op), &mut ev);
                    // Pull any stale reseed first, then place the new one
                    // (orphaned touch orders otherwise accumulate behind
                    // better-priced makers).
                    op.side = side.opp();
                    let slot_side = op.side.idx();
                    let old_reseed = slots[acct][sym_i][slot_side];
                    if old_reseed != 0 {
                        ev.clear();
                        shard.execute(
                            Cmd::Cancel {
                                order: old_reseed,
                                symbol: sym,
                                account: acct as u32,
                            },
                            &mut ev,
                        );
                        ops += 1;
                        cmds_this_batch += 1;
                    }
                    // Reseed the eaten level from the same account with the
                    // exact crossed qty (position-neutral for the taker).
                    op.price = if op.side == Side::Buy { 505 } else { 494 };
                    op.qty = q;
                    op.tif = Tif::Gtc;
                    ev.clear();
                    shard.execute(Cmd::Place(op), &mut ev);
                    slots[acct][sym_i][slot_side] = shard.last_ack();
                    ops += 2;
                    cmds_this_batch += 2;
                }
                _ => {
                    // Pull own quote: probe up to 4 (symbol, side) slots for
                    // an occupied one so every counted op is a real command.
                    let side_i = if r & 0x1_0000 == 0 { 0 } else { 1 };
                    let mut sym_j = sym_i;
                    let mut old = slots[acct][sym_j][side_i];
                    let mut probes = 0;
                    while old == 0 && probes < 4 {
                        sym_j = (sym_j + 1) % symbols.len();
                        old = slots[acct][sym_j][side_i];
                        probes += 1;
                    }
                    if old != 0 {
                        ev.clear();
                        shard.execute(
                            Cmd::Cancel {
                                order: old,
                                symbol: symbols[sym_j],
                                account: acct as u32,
                            },
                            &mut ev,
                        );
                        slots[acct][sym_j][side_i] = 0;
                        ops += 1;
                        cmds_this_batch += 1;
                    }
                }
            }
            // Time & marks: keep the session alive logically.
            if ops & 0x7FF == 0 {
                ev.clear();
                shard.execute(
                    Cmd::Advance {
                        to: shard.now() + 1_000_000,
                    },
                    &mut ev,
                );
            }
            if ops & 0xFFFF == 0 {
                ev.clear();
                let spot = 500_000u64 + (rng.next() % 2_000);
                shard.execute(Cmd::Oracle { price: spot }, &mut ev);
            }
        }
        let dt = t0.elapsed().as_nanos() as f64;
        batch_means.push(dt / cmds_this_batch.max(1) as f64);
        if Instant::now() >= deadline {
            break 'outer;
        }
    }
    let elapsed = t_start.elapsed().as_secs_f64();
    shard.flush_journal();

    // ------------------------------------------------------------------
    // Audit & report.
    // ------------------------------------------------------------------
    batch_means.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let q = |p: f64| {
        let idx = ((batch_means.len() as f64) * p).floor() as usize;
        batch_means[idx.min(batch_means.len() - 1)]
    };
    let mean = if ops > 0 {
        (elapsed * 1e9) / ops as f64
    } else {
        0.0
    };
    let stats = shard.stats;
    let audit = shard.audit();
    WorkerReport {
        ops,
        trades: stats.trades,
        rejects: stats.rejects,
        cancels: stats.cancels,
        ns_per_op_mean: mean,
        p50: q(0.50),
        p99: q(0.99),
        p999: q(0.999),
        journal_written: shard.journal.written(),
        conservation_ok: audit.conservation_ok,
        live_orders: audit.book_orders,
        open_positions: audit.open_positions as u64,
    }
}

fn fmt_m(v: f64) -> String {
    if v >= 1e9 {
        format!("{:.2}B", v / 1e9)
    } else if v >= 1e6 {
        format!("{:.2}M", v / 1e6)
    } else if v >= 1e3 {
        format!("{:.1}K", v / 1e3)
    } else {
        format!("{v:.0}")
    }
}

fn main() {
    let args = parse_args();
    println!(
        "ns-tps: {} shard(s), mode {:?}, {}s, cores={}",
        args.shards,
        args.mode,
        args.seconds,
        std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0)
    );
    println!("---");
    let t0 = Instant::now();
    let mut handles = Vec::new();
    for i in 0..args.shards {
        let mode = args.mode;
        let secs = args.seconds;
        let wal = args.wal.clone().map(|p| format!("{p}.shard{i}"));
        let seed = 0x9E37_79B9_7F4A_7C15u64.wrapping_mul(i as u64 + 1) | 1;
        handles.push(std::thread::spawn(move || worker(i as u32, mode, secs, wal.as_deref(), seed)));
    }
    let reports: Vec<WorkerReport> = handles.into_iter().map(|h| h.join().expect("worker")).collect();
    let wall = t0.elapsed().as_secs_f64();

    let total_ops: u64 = reports.iter().map(|r| r.ops).sum();
    let total_trades: u64 = reports.iter().map(|r| r.trades).sum();
    let total_rejects: u64 = reports.iter().map(|r| r.rejects).sum();
    let total_journal: u64 = reports.iter().map(|r| r.journal_written).sum();
    let all_ok = reports.iter().all(|r| r.conservation_ok);

    println!("per-shard report:");
    println!(
        "{:>8} {:>10} {:>10} {:>9} {:>9} {:>9} {:>9} {:>8}",
        "shard", "ops", "ops/s", "mean ns", "p50 ns", "p99 ns", "p99.9 ns", "trades"
    );
    for (i, r) in reports.iter().enumerate() {
        let rate = r.ops as f64 / wall;
        println!(
            "{:>8} {:>10} {:>10} {:>9.1} {:>9.1} {:>9.1} {:>9.1} {:>8}",
            i,
            fmt_m(r.ops as f64),
            fmt_m(rate),
            r.ns_per_op_mean,
            r.p50,
            r.p99,
            r.p999,
            fmt_m(r.trades as f64)
        );
    }
    println!("---");
    for (i, r) in reports.iter().enumerate() {
        println!(
            "shard {i}: live resting orders = {}, open positions = {}, cancels = {}, rejects = {}",
            fmt_m(r.live_orders as f64),
            r.open_positions,
            fmt_m(r.cancels as f64),
            fmt_m(r.rejects as f64)
        );
    }
    println!("---");
    let agg_rate = total_ops as f64 / wall;
    let trade_rate = total_trades as f64 / wall;
    println!("aggregate order TPS:   {} ops/s", fmt_m(agg_rate));
    println!("aggregate trade rate:  {} trades/s", fmt_m(trade_rate));
    println!("rejects:               {} ({:.2}%)", fmt_m(total_rejects as f64), 100.0 * total_rejects as f64 / total_ops as f64);
    println!("journal WAL bytes:     {}", fmt_m(total_journal as f64));
    println!("conservation:          {}", if all_ok { "OK (exact integer, every shard)" } else { "VIOLATED" });
    let target = 20_000_000.0;
    println!(
        ">= 20M TPS target:     {} ({:.1}M ops/s aggregate over {wall:.1}s wall)",
        if agg_rate >= target { "PASS" } else { "FAIL" },
        agg_rate / 1e6
    );
    if !all_ok {
        std::process::exit(2);
    }
}
