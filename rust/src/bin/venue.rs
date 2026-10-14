//! `ns-venue` — the production venue binary.
//!
//! Boots the engine shards (replaying the WAL if present), seeds accounts,
//! API keys and the session ladder, then serves HTTP + WebSocket on the
//! bind address until drained (`/admin/shutdown`, `--drain-after-secs`, or
//! process termination with WAL recovery on restart).
//!
//! Examples:
//! ```text
//! ns-venue --bind 127.0.0.1:8080 --demo
//! ns-venue --wal-dir /var/lib/ns-venue --api-key a1b2c3d4:<64-hex>:admin:1
//! ns-venue --loadtest --drain-after-secs 30
//! ```

use ns_clob::auth::{key_id_hex, parse_key_id, Scope};
use ns_clob::fees::Tier;
use ns_clob::types::Cash;
use ns_clob::venue::{LadderCfg, SeedKey, Venue, VenueConfig};
use std::path::PathBuf;
use std::time::Duration;

fn main() {
    let mut cfg = VenueConfig::default();
    let mut demo = false;
    let mut marks: Vec<u64> = Vec::new();
    let mut ladder_sessions: Option<u32> = None;
    let mut drain_after: Option<u64> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut val = |name: &str| -> String {
            args.next()
                .unwrap_or_else(|| panic!("{name} requires a value"))
        };
        match arg.as_str() {
            "--bind" => cfg.bind = val("--bind"),
            "--underlyings" => {
                let n: usize = val("--underlyings").parse().expect("count");
                cfg.underlyings = (0..n as u16).collect();
            }
            "--wal-dir" => cfg.wal_dir = Some(PathBuf::from(val("--wal-dir"))),
            "--api-key" => {
                // KEY:SECRET:SCOPE:ACCOUNT
                let spec = val("--api-key");
                let parts: Vec<&str> = spec.split(':').collect();
                if parts.len() != 4 {
                    eprintln!("--api-key must be KEY:SECRET:SCOPE:ACCOUNT");
                    std::process::exit(2);
                }
                let key_id = parse_key_id(parts[0])
                    .unwrap_or_else(|| panic!("key id must be 8 hex chars"));
                let scope = Scope::parse(parts[2])
                    .unwrap_or_else(|| panic!("scope must be read|trade|admin"));
                let account: u32 = parts[3].parse().expect("account id");
                cfg.seed_keys.push(SeedKey {
                    key_id,
                    secret_hex: parts[1].to_string(),
                    account,
                    scope,
                });
            }
            "--account" => {
                // ID:TIER
                let spec = val("--account");
                let parts: Vec<&str> = spec.split(':').collect();
                let id: u32 = parts[0].parse().expect("account id");
                let tier = match parts.get(1).copied() {
                    Some("silver") => Tier::Silver,
                    Some("gold") => Tier::Gold,
                    Some("prime") => Tier::Prime,
                    _ => Tier::Base,
                };
                cfg.seed_accounts.push((id, tier));
            }
            "--fund" => {
                // ACCOUNT:UNDERLYING:TICKS
                let spec = val("--fund");
                let parts: Vec<&str> = spec.split(':').collect();
                if parts.len() != 3 {
                    eprintln!("--fund must be ACCOUNT:UNDERLYING:TICKS");
                    std::process::exit(2);
                }
                let account: u32 = parts[0].parse().expect("account");
                let underlying: u16 = parts[1].parse().expect("underlying");
                let ticks: Cash = parts[2].parse().expect("ticks");
                cfg.seed_deposits.push((account, underlying, ticks));
            }
            "--mark" => {
                marks.push(val("--mark").parse().expect("ticks"));
            }
            "--ladder" => {
                ladder_sessions = Some(val("--ladder").parse().expect("sessions"));
            }
            "--ladder-step" => {
                let step: u64 = val("--ladder-step").parse().expect("ticks");
                cfg.ladder.get_or_insert_with(|| LadderCfg {
                    marks: Vec::new(),
                    sessions: 3,
                    strike_step: 500,
                    span_pct: 3,
                })
                .strike_step = step;
            }
            "--sim-feed" => cfg.sim_feed = true,
            "--loadtest" => cfg.loadtest = true,
            "--demo" => demo = true,
            "--drain-after-secs" => {
                drain_after = Some(val("--drain-after-secs").parse().expect("seconds"));
            }
            "--clock-ms" => cfg.clock_ms = val("--clock-ms").parse().expect("ms"),
            "--max-conns" => cfg.max_conns = val("--max-conns").parse().expect("n"),
            "--help" | "-h" => {
                println!(
                    "ns-venue — 0DTE options venue\n\
                     --bind HOST:PORT            listen address (default 127.0.0.1:8080)\n\
                     --underlyings N             trade underlyings 0..N-1 (default 1)\n\
                     --wal-dir DIR               file WAL + boot replay\n\
                     --api-key KEY:SECRET:SCOPE:ACCOUNT   seed an API key\n\
                     --account ID:TIER           create an account (base|silver|gold|prime)\n\
                     --fund ACC:UND:TICKS        deposit cash\n\
                     --mark TICKS                initial underlying mark (per underlying)\n\
                     --ladder N                  list N hourly 0DTE sessions at boot\n\
                     --ladder-step TICKS         strike increment (default 500)\n\
                     --sim-feed                  synthetic oracle marks every 100ms\n\
                     --loadtest                  lift engine rate caps (benchmarks only)\n\
                     --demo                      mint demo admin+trader keys (insecure)\n\
                     --drain-after-secs N        auto-shutdown (tests/demos)\n\
                     --clock-ms N                engine clock tick (default 10)\n\
                     --max-conns N               connection cap (default 512)"
                );
                std::process::exit(0);
            }
            other => {
                eprintln!("unknown flag {other} (see --help)");
                std::process::exit(2);
            }
        }
    }

    // Demo mode: one admin key, one funded trader key, 3-session ladder,
    // sim feed — everything printed once to stderr.
    if demo {
        let admin_secret = random_hex();
        let trader_secret = random_hex();
        cfg.seed_keys.push(SeedKey {
            key_id: 0x0000_0001,
            secret_hex: admin_secret.clone(),
            account: 1,
            scope: Scope::Admin,
        });
        cfg.seed_keys.push(SeedKey {
            key_id: 0x0000_0002,
            secret_hex: trader_secret.clone(),
            account: 2,
            scope: Scope::Trade,
        });
        cfg.seed_accounts.push((1, Tier::Prime));
        cfg.seed_accounts.push((2, Tier::Gold));
        for u in cfg.underlyings.clone() {
            cfg.seed_deposits.push((1, u, 100_000_000));
            cfg.seed_deposits.push((2, u, 10_000_000));
        }
        if marks.is_empty() {
            marks = vec![500_000];
        }
        ladder_sessions = ladder_sessions.or(Some(3));
        cfg.sim_feed = true;
        eprintln!("== demo keys (shown once) ==");
        eprintln!("admin : {} {}", key_id_hex(1), admin_secret);
        eprintln!("trader: {} {}", key_id_hex(2), trader_secret);
    }

    if !marks.is_empty() {
        let sessions = ladder_sessions.unwrap_or(3);
        let step = cfg
            .ladder
            .as_ref()
            .map(|l| l.strike_step)
            .unwrap_or(500);
        cfg.ladder = Some(LadderCfg {
            marks,
            sessions,
            strike_step: step,
            span_pct: 3,
        });
    }
    if let Some(secs) = drain_after {
        cfg.drain_after = Some(Duration::from_secs(secs));
    }

    let bind = cfg.bind.clone();
    let mut venue = match Venue::start(cfg) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("venue: failed to start: {e}");
            std::process::exit(1);
        }
    };
    eprintln!("ns-venue listening on {bind} ({} shards)", venue.nshards());

    // Block until the workers stop (drain-after or /admin/shutdown), then
    // drain cleanly and print the final audit.
    loop {
        if venue.ctx.stopping.load(std::sync::atomic::Ordering::Acquire) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    // Give in-flight requests a moment to complete.
    std::thread::sleep(Duration::from_millis(200));
    venue.drain();
    let m = venue.ctx.replica.shard_metrics();
    let total_orders: u64 = m.iter().map(|x| x.orders).sum();
    let total_trades: u64 = m.iter().map(|x| x.trades).sum();
    let conservation = venue.ctx.replica.conservation();
    eprintln!(
        "ns-venue drained: {total_orders} orders, {total_trades} trades, conservation={conservation}"
    );
    if !conservation {
        std::process::exit(2);
    }
}

fn random_hex() -> String {
    let mut buf = [0u8; 32];
    ns_clob::crypto::os_random(&mut buf);
    ns_clob::crypto::hex(&buf)
}
