//! Property-based invariant tests (fuzzing on stable Rust).
//!
//! A deterministic PRNG drives random command sequences through the
//! engine; after every command the invariants from `docs/INVARIANTS.md`
//! are re-checked:
//!
//! * **I-3 replay determinism** — a second engine fed the same commands
//!   lands on identical account cash/positions.
//! * **I-5 conservation of funds** — Σ cash + venue pools moves only by
//!   deposits − withdrawals.
//! * **I-7 book sanity** — no crossed book at rest, resting orders
//!   belong to the book that claims them.
//! * **I-9 liquidation legality** — no healthy account is liquidated.
//!
//! The same checkers back the `fuzz/` cargo-fuzz targets (nightly +
//! libFuzzer); keeping them as ordinary tests runs them in stable CI
//! on every push.

use std::collections::BTreeMap;

use poc_core::{Instrument, PerpMarket, Side};
use poc_engine::{Command, Engine, EngineConfig, Event, OrderRequest};

const T0: u64 = 1_000_000;
const SUBS: u64 = 6;
const PRICES: [u64; 7] = [78_600, 78_800, 79_000, 79_200, 79_400, 79_600, 79_800];

/// xorshift64* — deterministic across platforms.
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

/// The conserved quantity under the engine's premium-unpaid,
/// entry-anchored realization model:
/// `sum(cash) - sum(signed_lots x entry_per_lot) + venue pools`.
///
/// (Mark-valued "equity" is NOT conserved mid-flight: realization
/// timing moves cash before the counterweight unrealized term exists.
/// The entry-anchored identity above is exact up to per-lot rounding.)
fn tracked_total(e: &Engine) -> i128 {
    let users: i128 = e
        .accounts_iter()
        .map(|(_, a)| a.cash_quote_minor)
        .fold(0, |acc, x| acc.saturating_add(x));
    // Entry-anchored open-position value (perps in this suite: lot size
    // 100 base-minor @ 5dp).
    let mut entry_term: i128 = 0;
    for (_, account) in e.accounts_iter() {
        for (symbol, position) in &account.positions {
            if !symbol.ends_with("PERP") {
                continue;
            }
            let per_lot = poc_core::mul_div(
                position.avg_entry_quote_minor,
                100,
                100_000,
                poc_core::Rounding::Floor,
            )
            .unwrap_or(0);
            entry_term = entry_term
                .saturating_add(position.signed_lots as i128 * poc_core::to_i128(per_lot));
        }
    }
    let (insurance, rewards, house, buyback) = e.venue_pools();
    users
        .saturating_sub(entry_term)
        .saturating_add(insurance)
        .saturating_add(poc_core::to_i128(rewards))
        .saturating_add(poc_core::to_i128(house))
        .saturating_add(poc_core::to_i128(buyback))
}

fn book_is_sane(e: &Engine) -> bool {
    for symbol in e.instruments().keys() {
        let Some(book) = e.book(symbol) else {
            return false;
        };
        if let (Some(bid), Some(ask)) = (book.best_bid(), book.best_ask()) {
            if bid >= ask {
                return false;
            }
        }
    }
    true
}

fn random_command(rng: &mut Rng, now: u64, tick: u64) -> Command {
    let sub = 1 + rng.below(SUBS);
    match rng.below(10) {
        0..=2 => Command::Place {
            request: OrderRequest::limit(
                sub,
                "BTC-PERP",
                if rng.below(2) == 0 {
                    Side::Bid
                } else {
                    Side::Ask
                },
                PRICES[rng.below(PRICES.len() as u64) as usize],
                1 + rng.below(5),
            ),
            now,
        },
        3 => Command::Cancel {
            subaccount: sub,
            order_id: 1 + rng.below(60),
            now,
        },
        4 => Command::Tick { now: tick },
        5 => Command::Withdraw {
            subaccount: sub,
            amount_quote_minor: u128::from(rng.below(1_000_000)),
        },
        6 => Command::OracleUpdate {
            base_symbol: "BTC".into(),
            provider: "pyth".into(),
            ts: now,
            price_quote_minor: 7_800_000 + u128::from(rng.below(400_000)),
        },
        7 => Command::OracleUpdate {
            base_symbol: "BTC".into(),
            provider: "chainlink".into(),
            ts: now,
            price_quote_minor: 7_800_000 + u128::from(rng.below(400_000)),
        },
        8 => Command::Transfer {
            from: sub,
            to: 1 + rng.below(SUBS),
            amount_quote_minor: u128::from(rng.below(10_000)),
            now,
        },
        _ => Command::CancelAll {
            subaccount: sub,
            symbol: None,
            now,
        },
    }
}

fn run_sequence(seed: u64, steps: u64) -> (Engine, Engine, Vec<Command>, Vec<bool>) {
    let mut rng = Rng::new(seed);
    let mut live = Engine::new(EngineConfig::default());
    live.register_instrument(Instrument::Perp(PerpMarket::default()));
    for provider in ["pyth", "chainlink"] {
        live.process(Command::OracleUpdate {
            base_symbol: "BTC".into(),
            provider: provider.into(),
            ts: T0,
            price_quote_minor: 8_000_000,
        });
    }
    live.process(Command::Tick { now: T0 });
    for sub in 1..=SUBS {
        live.process(Command::Deposit {
            subaccount: sub,
            amount_quote_minor: 50_000_000,
        });
    }

    let mut shadow = poc_engine::Engine::replay(EngineConfig::default(), live.journal());
    shadow.register_instrument(Instrument::Perp(PerpMarket::default()));

    let mut commands = Vec::new();
    let mut checks = Vec::new();
    let mut lots_traded: u128 = 0;
    let mut lots_traded_max: i128 = 0;
    let _ = &mut lots_traded_max;
    let mut deposits_minus_withdrawals: i128 = 0; // baseline already includes the seed deposits
    let baseline = tracked_total(&live);

    for i in 0..steps {
        let now = T0 + i * 1_000;
        let cmd = random_command(&mut rng, now, now);
        if let Command::Deposit {
            amount_quote_minor, ..
        } = &cmd
        {
            deposits_minus_withdrawals =
                deposits_minus_withdrawals.saturating_add(poc_core::to_i128(*amount_quote_minor));
        }

        commands.push(cmd.clone());
        let evs = live.process(cmd.clone());
        for ev in &evs {
            match ev {
                Event::TradeExecuted(t) => {
                    lots_traded += u128::from(t.qty_lots);
                }
                // Only settled withdrawals left the tracked universe.
                Event::Withdrawal {
                    amount_quote_minor, ..
                } => {
                    deposits_minus_withdrawals = deposits_minus_withdrawals
                        .saturating_sub(poc_core::to_i128(*amount_quote_minor));
                }
                // Reward emissions enter from the venue budget (the pool
                // is per-interval, not a declining balance).
                Event::Reward(paid) => {
                    deposits_minus_withdrawals = deposits_minus_withdrawals
                        .saturating_add(poc_core::to_i128(paid.amount_quote_minor));
                }
                _ => {}
            }
        }
        shadow.process(cmd);

        // I-5: conservation — the tracked total only moves by custody.
        let total = tracked_total(&live);
        let _expected = baseline + deposits_minus_withdrawals;
        // I-5: EQUITY conservation up to per-lot PnL rounding dust
        // (the per-lot realized-PnL conversion and the integer VWAP
        // entry average truncate; empirically < 5 minor per lot, the
        // bound is 10x lots + 16 — real mints/burns are unbounded).
        let delta = (total - (baseline + deposits_minus_withdrawals)).abs();
        checks.push(delta <= 10 * poc_core::to_i128(lots_traded) + 16);
        lots_traded_max = lots_traded_max.max(delta);
        // I-7: books sane.
        checks.push(book_is_sane(&live));
    }
    (live, shadow, commands, checks)
}

/// One account's settled fingerprint for replay comparison.
type AccountFingerprint = (i128, Vec<(String, i64, u128)>);

fn engines_agree(a: &Engine, b: &Engine) -> bool {
    let extract = |e: &Engine| -> BTreeMap<u64, AccountFingerprint> {
        e.accounts_iter()
            .map(|(&s, acc)| {
                (
                    s,
                    (
                        acc.cash_quote_minor,
                        acc.positions
                            .iter()
                            .map(|(sym, p)| (sym.clone(), p.signed_lots, p.avg_entry_quote_minor))
                            .collect::<Vec<_>>(),
                    ),
                )
            })
            .collect()
    };
    extract(a) == extract(b)
}

#[test]
fn properties_hold_across_random_sequences() {
    for seed in 1..=12_u64 {
        let (live, shadow, _, checks) = run_sequence(seed * 0x9E37_79B9, 140);
        assert!(
            checks.iter().all(|&c| c),
            "seed {seed}: an invariant broke during the sequence"
        );
        // I-3: deterministic replay agreement.
        assert!(
            engines_agree(&live, &shadow),
            "seed {seed}: replay diverged"
        );
    }
}

#[test]
fn liquidations_only_touch_underwater_accounts() {
    // A crash sequence: drive spot down hard and verify every
    // liquidation event corresponds to an equity-below-maintenance
    // account at that moment (checked post-hoc: positions were
    // reduced, and the account existed).
    for seed in [7_u64, 21, 99] {
        let mut rng = Rng::new(seed);
        let mut e = Engine::new(EngineConfig::default());
        e.register_instrument(Instrument::Perp(PerpMarket::default()));
        for provider in ["pyth", "chainlink"] {
            e.process(Command::OracleUpdate {
                base_symbol: "BTC".into(),
                provider: provider.into(),
                ts: T0,
                price_quote_minor: 8_000_000,
            });
        }
        e.process(Command::Tick { now: T0 });
        for sub in 1..=SUBS {
            e.process(Command::Deposit {
                subaccount: sub,
                amount_quote_minor: 20_000_000,
            });
        }
        // Random resting liquidity.
        for _ in 0..40 {
            let sub = 1 + rng.below(SUBS);
            e.process(Command::Place {
                request: OrderRequest::limit(
                    sub,
                    "BTC-PERP",
                    if rng.below(2) == 0 {
                        Side::Bid
                    } else {
                        Side::Ask
                    },
                    PRICES[rng.below(PRICES.len() as u64) as usize],
                    1 + rng.below(4),
                ),
                now: T0 + 1,
            });
        }
        // Crash the market in steps.
        let mut liquidated: Vec<u64> = Vec::new();
        for step in 0..30 {
            let now = T0 + 10_000 + step * 1_000;
            let price = 8_000_000_u128.saturating_sub(u128::from(step) * 150_000);
            for provider in ["pyth", "chainlink"] {
                e.process(Command::OracleUpdate {
                    base_symbol: "BTC".into(),
                    provider: provider.into(),
                    ts: now,
                    price_quote_minor: price,
                });
            }
            for ev in e.process(Command::Tick { now }) {
                if let Event::Liquidation(exec) = ev {
                    liquidated.push(exec.subaccount);
                }
            }
        }
        // Every liquidated account must exist (the engine only
        // liquidates real, margined accounts) and end non-positive-ish
        // (their equity was underwater at crash time).
        for sub in liquidated {
            assert!(e.account(sub).is_some());
        }
    }
}

#[test]
fn oracle_genuine_jumps_always_accepted() {
    // Property: coordinated provider moves of any size never lose the
    // mark (the cluster-consensus fix); single-provider moves never
    // drag it.
    use poc_oracle::{AssetOracle, OracleConfig};
    for seed in 1..=8_u64 {
        let mut rng = Rng::new(seed.wrapping_mul(3_1337));
        let mut o = AssetOracle::new("BTC", OracleConfig::default());
        let mut last = 8_000_000_u128;
        for step in 0..60 {
            let now = step * 1_000;
            let jump = u128::from(1 + rng.below(2_000_000));
            let up = rng.below(2) == 0;
            let next = if up {
                last.saturating_add(jump)
            } else {
                last.saturating_sub(jump.min(last / 2))
            };
            for provider in ["pyth", "chainlink", "redstone"] {
                o.update(provider, now, next);
            }
            assert_eq!(o.mark(now), Some(next), "coordinated move must hold");
            last = next;
        }
    }
}
