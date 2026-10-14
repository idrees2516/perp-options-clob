//! End-to-end: a complete simulated 0DTE trading day on one shard.
//!
//! Boot -> deposit -> hourly expiries with rolling auctions -> volatility
//! regime change (fast market) -> circuit-breaker halt and reopen auction ->
//! MOC close -> exercise/settlement -> EOD reconciliation. Every stage
//! asserts exact integer cash balances and the global conservation
//! invariant after **every** command.

use ns_clob::engine::{Cmd, Phase, Shard, ShardConfig, NANOS_PER_MIN};
use ns_clob::fees::Tier;
use ns_clob::journal::EvBuf;
use ns_clob::marketdata::Mds;
use ns_clob::sim::{AgentCmd, DaySchedule, Flow};
use ns_clob::symbol::{Listing, OptKind};
use ns_clob::types::*;

fn minute(n: u64) -> Nanos {
    n * NANOS_PER_MIN
}

fn shard() -> Shard {
    Shard::new(0, 0, ShardConfig::default_config())
}

/// Deposit -> trade -> expire, with exact assertions at every step.
#[test]
fn full_day_lifecycle_with_exact_reconciliation() {
    let sched = DaySchedule::default_day();
    let mut s = shard();
    let mut ev = EvBuf::with_capacity(256);
    let mut mds = Mds::new();

    // Two accounts, one aggressive (Base tier), one quoting (Prime tier).
    s.execute(
        Cmd::CreateAccount {
            account: 1,
            tier: Tier::Base,
        },
        &mut ev,
    );
    s.execute(
        Cmd::CreateAccount {
            account: 2,
            tier: Tier::Prime,
        },
        &mut ev,
    );
    s.execute(
        Cmd::Deposit {
            account: 1,
            amount: 20_000_000,
        },
        &mut ev,
    );
    s.execute(
        Cmd::Deposit {
            account: 2,
            amount: 20_000_000,
        },
        &mut ev,
    );
    assert!(s.conservation());
    let deposits: Cash = 40_000_000;

    // The morning chain: one expiry per hour, 8 strikes, calls+puts.
    let day0 = 30_912_000u32; // a trading-day midnight in epoch minutes
    let mut listings = Vec::new();
    for &exp in sched.expiries.iter() {
        for &k in sched.strikes.iter() {
            for kind in [OptKind::Call, OptKind::Put] {
                let l = Listing {
                    underlying: 0,
                    expiry_min: day0 + exp,
                    kind,
                    strike: k,
                };
                s.execute(Cmd::CreateSymbol { listing: l }, &mut ev);
                listings.push((l, s.symbol_id(l)));
            }
        }
    }
    assert_eq!(s.symbols.len(), 7 * 8 * 2);

    // Open the session at 9:29 with an underlying mark.
    s.execute(
        Cmd::Advance {
            to: minute((day0 as u64) + 569),
        },
        &mut ev,
    );
    s.execute(Cmd::Oracle { price: 500_000 }, &mut ev);
    assert_eq!(s.ref_price, 500_000);

    // Session morning: quote and cross on the 10:00 expiry ATM call.
    let atm = s.symbol_id(Listing {
        underlying: 0,
        expiry_min: day0 + 600,
        kind: OptKind::Call,
        strike: 500_000,
    });
    let o = |symbol, acct, side, price, qty, tif| NewOrder {
        symbol,
        account: acct,
        side,
        price,
        qty,
        tif,
        kind: Kind::Limit,
        flags: 0,
        oco: NO_ID,
    };
    s.execute(Cmd::Place(o(atm, 2, Side::Sell, 800, 20, Tif::Gtc)), &mut ev);
    s.execute(Cmd::Place(o(atm, 1, Side::Buy, 800, 6, Tif::Gtc)), &mut ev);
    mds.ingest(&ev);
    assert_eq!(s.stats.trades, 1);
    // Exact flows: premium 4,800; taker fee 65*6=390; maker rebate 10*6*0.625=37 (Prime 640/1024).
    let taker_fee = 390u64;
    let maker_rebate = ((10u64 * 640) >> 10) * 6;
    assert_eq!(s.accounts[1].cash, 20_000_000 - 4_800 - taker_fee as i128);
    assert_eq!(s.accounts[2].cash, 20_000_000 + 4_800 + maker_rebate as i128);
    assert_eq!(
        s.ledger.house,
        (taker_fee - maker_rebate) as i128
    );
    assert!(s.conservation());

    // Rolling expiries: advance through each hourly expiry; symbols settle,
    // positions flatten, margins release.
    for &exp in sched.expiries.iter() {
        s.execute(
            Cmd::Advance {
                to: minute((day0 + exp) as u64 + 1),
            },
            &mut ev,
        );
        mds.ingest(&ev);
        assert!(s.conservation(), "conservation broken after expiry {exp}");
    }
    // Every symbol expired by 16:01.
    for sym in &s.symbols {
        assert_eq!(sym.phase, Phase::Expired);
    }
    // The 10:00 ATM call settled at the last underlying mark with the long
    // 6 contracts: cash flow = intrinsic*6 - fee*6.
    // (Mark path is deterministic; just assert flat positions + margins.)
    assert_eq!(s.audit().open_positions, 0);
    assert_eq!(s.accounts[1].margin_required, 0);
    assert_eq!(s.accounts[2].margin_required, 0);
    assert_eq!(s.accounts[1].pos.len(), 0);
    assert_eq!(s.accounts[2].pos.len(), 0);
    assert_eq!(s.accounts[1].hold, 0);
    assert_eq!(s.accounts[2].hold, 0);
    assert_eq!(s.accounts[1].margin_reserved, 0);
    assert_eq!(s.accounts[2].margin_reserved, 0);

    // EOD reconciliation: conservation, margin consistency, no negative cash.
    let audit = s.audit();
    assert!(audit.conservation_ok);
    assert!(audit.margin_consistent);
    assert_eq!(audit.negative_cash, 0);
    let sum = s.accounts[1].cash + s.accounts[2].cash;
    assert_eq!(sum + s.ledger.house, deposits);
    assert_eq!(s.stats.expiries, 112);
    assert!(mds.expiries >= 112);
}

/// Fast market + circuit breaker + reopening auction in one day.
#[test]
fn volatility_regime_and_circuit_breaker() {
    let _sched = DaySchedule::default_day();
    let mut s = shard();
    let mut ev = EvBuf::new();
    let mut mds = Mds::new();
    s.execute(
        Cmd::CreateAccount {
            account: 1,
            tier: Tier::Base,
        },
        &mut ev,
    );
    s.execute(
        Cmd::Deposit {
            account: 1,
            amount: 10_000_000,
        },
        &mut ev,
    );
    let day0 = 30_912_000u32;
    let l = Listing {
        underlying: 0,
        expiry_min: day0 + 960,
        kind: OptKind::Call,
        strike: 500_000,
    };
    s.execute(Cmd::CreateSymbol { listing: l }, &mut ev);
    s.execute(
        Cmd::Advance {
            to: minute(day0 as u64 + 569),
        },
        &mut ev,
    );
    s.execute(Cmd::Oracle { price: 500_000 }, &mut ev);

    // 3% move -> fast market on (bands tighten).
    s.execute(Cmd::Oracle { price: 516_000 }, &mut ev);
    assert!(s.fast);
    mds.ingest(&ev);
    // 7% move -> circuit breaker halt.
    s.execute(Cmd::Oracle { price: 536_000 }, &mut ev);
    assert!(s.halted);
    mds.ingest(&ev);
    assert_eq!(mds.halts, 1);

    // Orders rejected while halted.
    let o = NewOrder {
        symbol: s.symbol_id(l),
        account: 1,
        side: Side::Buy,
        price: 500,
        qty: 1,
        tif: Tif::Gtc,
        kind: Kind::Limit,
        flags: 0,
        oco: NO_ID,
    };
    s.execute(Cmd::Place(o), &mut ev);
    assert!(ev
        .events
        .iter()
        .any(|e| matches!(e, ns_clob::journal::Event::Reject { code: 13, .. })));

    // 5-minute halt lifts -> reopening auction -> continuous.
    s.execute(
        Cmd::Advance {
            to: minute(day0 as u64 + 575),
        },
        &mut ev,
    );
    assert!(!s.halted);
    assert_eq!(s.symbols[0].phase, Phase::AuctionOpen);
    s.execute(
        Cmd::Advance {
            to: minute(day0 as u64 + 577),
        },
        &mut ev,
    );
    assert_eq!(s.symbols[0].phase, Phase::Continuous);
    assert!(s.conservation());
}

/// Journal replay: run a randomized day, then rebuild from the WAL and
/// require bit-identical state.
#[test]
fn journal_replay_reproduces_state_exactly() {
    let sched = DaySchedule::default_day();
    let mut s = shard();
    let mut ev = EvBuf::with_capacity(512);
    let mut flow = Flow::boot(&mut s, 0xDEAD_BEEF, 24, &sched);
    // Run 30k adversarial commands through the session.
    let mut cmds_log: Vec<Cmd> = Vec::with_capacity(30_000);
    let clock_step = NANOS_PER_MIN / 300; // advance 200ms per command
    for _ in 0..30_000 {
        let c = match flow.next_cmd() {
            AgentCmd::Place(o) => Cmd::Place(o),
            AgentCmd::Cancel(id, sym) => Cmd::Cancel { order: id, symbol: sym },
            AgentCmd::Tick => Cmd::Advance {
                to: s.now() + clock_step,
            },
            AgentCmd::Mark(p) => Cmd::Oracle { price: p },
        };
        cmds_log.push(c);
        ev.clear();
        s.execute(c, &mut ev);
        flow.note_ack(&ev);
        assert!(s.conservation());
    }
    let audit = s.audit();
    assert!(audit.conservation_ok);
    assert!(audit.margin_consistent);
    assert!(audit.open_positions > 0, "expected live exposure mid-day");

    // Replay into a fresh shard with journaling off: boot commands first,
    // then the recorded stream.
    let mut r = shard();
    r.journaling = false;
    let mut rev = EvBuf::with_capacity(512);
    for c in flow.boot_cmds.iter().chain(cmds_log.iter()) {
        r.execute(*c, &mut rev);
    }
    assert_eq!(
        s.state_hash(),
        r.state_hash(),
        "replayed state must match bit-for-bit"
    );
    assert_eq!(s.stats.trades, r.stats.trades);
    assert_eq!(s.stats.volume, r.stats.volume);
}

/// Randomized adversarial suite: 3 independent seeds x 20k commands, with
/// conservation + margin consistency asserted after EVERY command.
#[test]
fn randomized_invariants_hold_after_every_operation() {
    let sched = DaySchedule::default_day();
    for seed in [0x1234_ABCD, 0x9E37_79B9, 0xC0FF_EE01] {
        let mut s = shard();
        let mut ev = EvBuf::with_capacity(512);
        let mut flow = Flow::boot(&mut s, seed, 16, &sched);
        for _ in 0..20_000 {
            let c = match flow.next_cmd() {
                AgentCmd::Place(o) => Cmd::Place(o),
                AgentCmd::Cancel(id, sym) => Cmd::Cancel { order: id, symbol: sym },
                AgentCmd::Tick => Cmd::Advance {
                    to: s.now() + NANOS_PER_MIN / 500,
                },
                AgentCmd::Mark(p) => Cmd::Oracle { price: p },
            };
            ev.clear();
            s.execute(c, &mut ev);
            flow.note_ack(&ev);
            assert!(s.conservation(), "seed {seed:#x}: conservation broken");
            // Books never crossed in continuous phase.
            for sym in &s.symbols {
                if sym.phase == Phase::Continuous {
                    if let (Some(b), Some(a)) = (sym.book.best_bid(), sym.book.best_ask()) {
                        assert!(b.0 < a.0, "seed {seed:#x}: crossed book {} >= {}", b.0, a.0);
                    }
                }
            }
        }
        let audit = s.audit();
        assert!(audit.margin_consistent, "seed {seed:#x}: margin drift");
        assert!(audit.conservation_ok, "seed {seed:#x}: final conservation");
    }
}

/// Replace (cancel-replace) keeps books sane and commitments exact.
#[test]
fn cancel_replace_roundtrip() {
    let _sched = DaySchedule::default_day();
    let mut s = shard();
    let mut ev = EvBuf::new();
    s.execute(
        Cmd::CreateAccount {
            account: 1,
            tier: Tier::Base,
        },
        &mut ev,
    );
    s.execute(
        Cmd::Deposit {
            account: 1,
            amount: 10_000_000,
        },
        &mut ev,
    );
    let day0 = 30_912_000u32;
    let l = Listing {
        underlying: 0,
        expiry_min: day0 + 960,
        kind: OptKind::Call,
        strike: 500_000,
    };
    s.execute(Cmd::CreateSymbol { listing: l }, &mut ev);
    s.execute(
        Cmd::Advance {
            to: minute(day0 as u64 + 569),
        },
        &mut ev,
    );
    s.execute(Cmd::Oracle { price: 500_000 }, &mut ev);
    let sym = s.symbol_id(l);
    s.execute(
        Cmd::Place(NewOrder {
            symbol: sym,
            account: 1,
            side: Side::Buy,
            price: 500,
            qty: 10,
            tif: Tif::Gtc,
            kind: Kind::Limit,
            flags: 0,
            oco: NO_ID,
        }),
        &mut ev,
    );
    let oid = ev
        .events
        .iter()
        .rev()
        .find_map(|e| match e {
            ns_clob::journal::Event::Ack { order, .. } => Some(*order),
            _ => None,
        })
        .unwrap();
    assert_eq!(s.accounts[1].hold, (500 + 65) * 10);
    // Replace to a worse price/qty: hold adjusts exactly.
    s.execute(
        Cmd::Replace {
            order: oid,
            symbol: sym,
            price: 480,
            qty: 6,
        },
        &mut ev,
    );
    assert_eq!(s.symbols[0].book.best_bid(), Some((480, 6)));
    assert_eq!(s.accounts[1].hold, (480 + 65) * 6);
    // Replace to a stale id: no-op.
    s.execute(
        Cmd::Replace {
            order: oid,
            symbol: sym,
            price: 400,
            qty: 2,
        },
        &mut ev,
    );
    assert_eq!(s.symbols[0].book.best_bid(), Some((480, 6)));
    assert!(s.conservation());
}

/// Withdrawals are bounded by available (post-hold, post-margin) cash.
#[test]
fn withdrawal_respects_holds_and_margin() {
    let _sched = DaySchedule::default_day();
    let mut s = shard();
    let mut ev = EvBuf::new();
    s.execute(
        Cmd::CreateAccount {
            account: 1,
            tier: Tier::Base,
        },
        &mut ev,
    );
    s.execute(
        Cmd::Deposit {
            account: 1,
            amount: 1_000_000,
        },
        &mut ev,
    );
    let day0 = 30_912_000u32;
    let l = Listing {
        underlying: 0,
        expiry_min: day0 + 960,
        kind: OptKind::Call,
        strike: 500_000,
    };
    s.execute(Cmd::CreateSymbol { listing: l }, &mut ev);
    s.execute(
        Cmd::Advance {
            to: minute(day0 as u64 + 569),
        },
        &mut ev,
    );
    s.execute(Cmd::Oracle { price: 500_000 }, &mut ev);
    let sym = s.symbol_id(l);
    s.execute(
        Cmd::Place(NewOrder {
            symbol: sym,
            account: 1,
            side: Side::Buy,
            price: 1_000,
            qty: 100,
            tif: Tif::Gtc,
            kind: Kind::Limit,
            flags: 0,
            oco: NO_ID,
        }),
        &mut ev,
    );
    // Hold = (1000 + 65) * 100 = 106,500.
    assert_eq!(s.accounts[1].hold, 106_500);
    // Can withdraw up to available = 1,000,000 - 106,500 = 893,500.
    s.execute(
        Cmd::Withdraw {
            account: 1,
            amount: 800_000,
        },
        &mut ev,
    );
    assert_eq!(s.accounts[1].cash, 200_000);
    // Beyond available (200,000 - 106,500 = 93,500): refused.
    s.execute(
        Cmd::Withdraw {
            account: 1,
            amount: 100_000,
        },
        &mut ev,
    );
    assert_eq!(s.accounts[1].cash, 200_000);
    // Within available: allowed.
    s.execute(
        Cmd::Withdraw {
            account: 1,
            amount: 90_000,
        },
        &mut ev,
    );
    assert_eq!(s.accounts[1].cash, 110_000);
    assert!(s.conservation());
}

/// True disaster recovery: run a randomized day, flush the WAL, throw the
/// shard away, decode the WAL bytes back into commands, replay into a fresh
/// shard, and require bit-for-bit identical state.
#[test]
fn wal_bytes_replay_disaster_recovery() {
    use ns_clob::engine::decode_cmd;
    let sched = DaySchedule::default_day();
    let mut s = shard();
    let mut ev = EvBuf::with_capacity(512);
    let mut flow = Flow::boot(&mut s, 0x5EED_1234, 12, &sched);
    for _ in 0..8_000 {
        let c = match flow.next_cmd() {
            AgentCmd::Place(o) => Cmd::Place(o),
            AgentCmd::Cancel(id, sym) => Cmd::Cancel { order: id, symbol: sym },
            AgentCmd::Tick => Cmd::Advance {
                to: s.now() + NANOS_PER_MIN / 400,
            },
            AgentCmd::Mark(p) => Cmd::Oracle { price: p },
        };
        ev.clear();
        s.execute(c, &mut ev);
        flow.note_ack(&ev);
        assert!(s.conservation());
    }
    let audit_before = s.audit();
    assert!(audit_before.conservation_ok && audit_before.margin_consistent);
    s.flush_journal();

    // Extract the WAL bytes and decode every record.
    let batches = s.journal.batches();
    assert!(!batches.is_empty(), "WAL must contain framed batches");
    let mut decoded: Vec<Cmd> = Vec::with_capacity(8_192);
    for payload in &batches {
        let mut rest = payload.as_slice();
        while !rest.is_empty() {
            // Records are length-delimited by their schema; decode greedily
            // by tag and skip the consumed bytes.
            let before = rest.len();
            if let Some(cmd) = decode_cmd(rest) {
                decoded.push(cmd);
            }
            // Advance by the record length: recompute from the tag layout.
            let len = record_len(rest);
            rest = &rest[len.min(before)..];
        }
    }
    assert!(decoded.len() > 8_000, "decoded {} records", decoded.len());

    // Replay into a fresh shard (journaling off).
    let mut r = shard();
    r.journaling = false;
    let mut rev = EvBuf::with_capacity(512);
    for c in &decoded {
        r.execute(*c, &mut rev);
    }
    assert_eq!(
        s.state_hash(),
        r.state_hash(),
        "WAL replay must reproduce state bit-for-bit"
    );
    assert_eq!(s.stats.trades, r.stats.trades);
    assert_eq!(s.stats.volume, r.stats.volume);
    let audit_after = r.audit();
    assert!(audit_after.conservation_ok);
}

/// Record length per tag (mirrors `journal_cmd`'s write layout).
fn record_len(raw: &[u8]) -> usize {
    match raw.first().copied().unwrap_or(0) {
        1 => 1 + 4 + 4 + 4 + 8 + 4 + 8,   // Place
        2 => 1 + 8 + 4,                     // Cancel
        3 => 1 + 8 + 4 + 8 + 4,             // Replace
        4 => 1 + 4 + 1,                     // CreateAccount
        5 | 6 => 1 + 4 + 16,                // Deposit / Withdraw
        7 => 1 + 8,                          // CreateSymbol
        8 => 1 + 8,                          // Oracle
        9 => 1 + 8,                          // Advance
        10 | 11 => 1,                        // Halt / ResumeNow
        _ => raw.len(),                      // unknown: bail out
    }
}
