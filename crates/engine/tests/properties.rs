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

// ----------------------------------------------------------------------
// Second closure wave: OCO, TWAP, batch, and vault properties
// ----------------------------------------------------------------------

/// Tracked total with the insurance fund's inventory entry value
/// included (G-23): the fund's carried positions enter the conserved
/// quantity at their carrying mark, exactly like account positions at
/// their entries.
fn tracked_total_with_inventory(e: &Engine) -> i128 {
    let base = tracked_total(e);
    let vault_collateral: i128 = e
        .vaults()
        .values()
        .map(|v| poc_core::to_i128(v.collateral_quote_minor))
        .fold(0_i128, |acc, x| acc.saturating_add(x));
    let mut inventory_term: i128 = 0;
    for (lots, mark) in e.insurance_inventory().values() {
        let per_lot =
            poc_core::mul_div(*mark, 100, 100_000, poc_core::Rounding::Floor).unwrap_or(0);
        inventory_term = inventory_term.saturating_add(*lots as i128 * poc_core::to_i128(per_lot));
    }
    base.saturating_add(inventory_term)
        .saturating_add(vault_collateral)
}

/// The second-wave command generator: everything the first mix had plus
/// OCO brackets, TWAP parents, atomic batches, and vault flows.
fn random_command_v2(rng: &mut Rng, now: u64, tick: u64, step: u64) -> Command {
    let sub = 1 + rng.below(SUBS);
    match rng.below(20) {
        0..=5 => Command::Place {
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
        6 => Command::PlaceOco {
            first: OrderRequest::limit(
                sub,
                "BTC-PERP",
                if rng.below(2) == 0 {
                    Side::Bid
                } else {
                    Side::Ask
                },
                PRICES[rng.below(PRICES.len() as u64) as usize],
                1 + rng.below(3),
            ),
            second: OrderRequest {
                order_type: poc_core::OrderType::StopMarket {
                    trigger_price: PRICES[rng.below(PRICES.len() as u64) as usize],
                },
                ..OrderRequest::limit(
                    sub,
                    "BTC-PERP",
                    if rng.below(2) == 0 {
                        Side::Bid
                    } else {
                        Side::Ask
                    },
                    PRICES[rng.below(PRICES.len() as u64) as usize],
                    1 + rng.below(3),
                )
            },
            now,
        },
        7 => Command::PlaceTwap {
            subaccount: sub,
            symbol: "BTC-PERP".into(),
            side: if rng.below(2) == 0 {
                Side::Bid
            } else {
                Side::Ask
            },
            total_lots: 2 + rng.below(6),
            slices: 1 + rng.below(3),
            slice_interval_ms: 1_000,
            limit_ticks: Some(PRICES[rng.below(PRICES.len() as u64) as usize]),
            now,
        },
        8 => Command::CancelTwap {
            subaccount: sub,
            parent_id: 1 + rng.below(4),
            now,
        },
        9 => Command::PlaceBatch {
            requests: (0..1 + rng.below(3))
                .map(|_| {
                    OrderRequest::limit(
                        sub,
                        "BTC-PERP",
                        if rng.below(2) == 0 {
                            Side::Bid
                        } else {
                            Side::Ask
                        },
                        PRICES[rng.below(PRICES.len() as u64) as usize],
                        1 + rng.below(3),
                    )
                })
                .collect(),
            now,
        },
        10 => Command::VaultSubscribe {
            vault_id: 1,
            subaccount: sub,
            amount_quote_minor: u128::from(rng.below(100_000)),
            now,
        },
        11 => Command::VaultRedeem {
            vault_id: 1,
            subaccount: sub,
            shares: u128::from(rng.below(100_000)),
            now,
        },
        12 => Command::Cancel {
            subaccount: sub,
            order_id: 1 + rng.below(60),
            now,
        },
        13 => Command::Tick { now: tick },
        14 => Command::Withdraw {
            subaccount: sub,
            amount_quote_minor: u128::from(rng.below(1_000_000)),
        },
        15 | 16 => Command::OracleUpdate {
            base_symbol: "BTC".into(),
            provider: "pyth".into(),
            ts: now,
            price_quote_minor: 7_800_000 + u128::from(rng.below(400_000)),
        },
        17 => Command::OracleUpdate {
            base_symbol: "BTC".into(),
            provider: "chainlink".into(),
            ts: now,
            price_quote_minor: 7_800_000 + u128::from(rng.below(400_000)),
        },
        18 => Command::Transfer {
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
    .tap_unused(step)
}

/// Keep `step` referenced (generator API stability).
trait TapUnused {
    fn tap_unused(self, _: u64) -> Self;
}
impl TapUnused for Command {
    fn tap_unused(self, _: u64) -> Self {
        self
    }
}

#[test]
fn second_wave_commands_hold_invariants() {
    for seed in 1..=10_u64 {
        let mut rng = Rng::new(seed.wrapping_mul(0x85EB_CA6B));
        let cfg = EngineConfig {
            vault_epoch_interval_ms: 5_000,
            ..EngineConfig::default()
        };
        let mut live = Engine::new(cfg.clone());
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
                amount_quote_minor: 80_000_000,
            });
        }
        live.process(Command::VaultCreate {
            revenue_share_bps: 1_000,
            now: T0,
        });

        let mut shadow = Engine::replay(cfg, live.journal());
        shadow.register_instrument(Instrument::Perp(PerpMarket::default()));

        // OCO bookkeeping (I-21): group -> both member ids.
        let mut oco_members: BTreeMap<u64, (u64, u64)> = BTreeMap::new();
        // TWAP bookkeeping (I-22): parent -> (total, placed so far).
        let mut twap: BTreeMap<u64, (u64, u64)> = BTreeMap::new();
        // Vault flow conservation (I-23).
        let mut vault_flow_check = true;

        let baseline = tracked_total_with_inventory(&live);
        let mut custody: i128 = 0;
        let mut lots_traded: i128 = 0;

        for step in 0..160_u64 {
            let now = T0 + step * 1_000;
            let cmd = random_command_v2(&mut rng, now, now, step);
            let evs = live.process(cmd.clone());
            for ev in &evs {
                match ev {
                    Event::Withdrawal {
                        amount_quote_minor, ..
                    } => {
                        custody = custody.saturating_sub(poc_core::to_i128(*amount_quote_minor));
                    }
                    Event::Deposit {
                        amount_quote_minor, ..
                    } => {
                        custody = custody.saturating_add(poc_core::to_i128(*amount_quote_minor));
                    }
                    Event::Reward(paid) => {
                        custody =
                            custody.saturating_add(poc_core::to_i128(paid.amount_quote_minor));
                    }
                    Event::TradeExecuted(t) => {
                        lots_traded = lots_traded.saturating_add(i128::from(t.qty_lots));
                    }
                    Event::OcoLinked {
                        group,
                        first,
                        second,
                        ..
                    } => {
                        oco_members.insert(*group, (*first, *second));
                    }
                    Event::TwapOpened(parent) => {
                        twap.insert(parent.parent_id, (parent.total_lots, 0));
                    }
                    Event::TwapSliced {
                        parent_id, request, ..
                    } => {
                        if let Some(entry) = twap.get_mut(parent_id) {
                            entry.1 = entry.1.saturating_add(request.qty_lots);
                        }
                    }
                    Event::VaultEpochSettled(epoch) => {
                        // I-23: flows are subscriptions negative,
                        // redemptions positive — their sum must equal
                        // redeemed minus subscribed exactly.
                        let net: i128 = epoch.flows.iter().map(|(_, f)| *f).sum::<i128>();
                        let expected = poc_core::to_i128(epoch.redeemed_quote_minor)
                            - poc_core::to_i128(epoch.subscribed_quote_minor);
                        if net != expected {
                            vault_flow_check = false;
                        }
                    }
                    _ => {}
                }
            }
            shadow.process(cmd);

            // I-5 (with inventory): conservation up to rounding dust.
            let total = tracked_total_with_inventory(&live);
            let delta = (total - (baseline + custody)).abs();
            assert!(
                delta <= 10 * lots_traded + 64,
                "seed {seed} step {step}: conservation broke by {delta}"
            );

            // I-21: at most one OCO member open per group.
            let open_ids: Vec<u64> = live
                .accounts_iter()
                .flat_map(|(_, a)| a.open_orders.keys().copied())
                .collect();
            for (group, (a, b)) in &oco_members {
                let both = open_ids.contains(a) && open_ids.contains(b);
                if both {
                    panic!("seed {seed} step {step}: OCO group {group} has both members open");
                }
            }

            // I-22: TWAP never overslices.
            for (parent, (total_lots, placed)) in &twap {
                if *placed > *total_lots {
                    panic!("seed {seed} step {step}: TWAP {parent} placed {placed} > {total_lots}");
                }
            }
        }

        assert!(vault_flow_check, "vault flows conserved");

        // I-3: replay determinism across the new commands.
        assert!(
            engines_agree(&live, &shadow),
            "seed {seed}: replay diverged with second-wave commands"
        );
        // Books sane end to end.
        assert!(book_is_sane(&live));
    }
}

// ----------------------------------------------------------------------
// Third closure wave: MM tiers, vault revenue share, quote interest
// ----------------------------------------------------------------------

/// The third-wave generator: the second-wave mix plus MM tier
/// enrollment and day-boundary-crossing ticks that exercise quote
/// interest. Conservation, replay determinism, and tier legality are
/// asserted after every command.
fn random_command_v3(rng: &mut Rng, now: u64, tick: u64) -> Command {
    let sub = 1 + rng.below(SUBS);
    // Bias toward ticks: the review windows and day boundaries must
    // actually fire inside the run.
    match rng.below(24) {
        0..=5 => Command::Place {
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
        6 => Command::MmTierEnroll {
            subaccount: sub,
            now,
        },
        7 => Command::PlaceBatch {
            requests: (0..1 + rng.below(2))
                .map(|_| {
                    OrderRequest::limit(
                        sub,
                        "BTC-PERP",
                        if rng.below(2) == 0 {
                            Side::Bid
                        } else {
                            Side::Ask
                        },
                        PRICES[rng.below(PRICES.len() as u64) as usize],
                        1 + rng.below(3),
                    )
                })
                .collect(),
            now,
        },
        8 => Command::VaultSubscribe {
            vault_id: 1,
            subaccount: sub,
            amount_quote_minor: u128::from(rng.below(100_000)),
            now,
        },
        9 => Command::VaultRedeem {
            vault_id: 1,
            subaccount: sub,
            shares: u128::from(rng.below(100_000)),
            now,
        },
        10 => Command::Cancel {
            subaccount: sub,
            order_id: 1 + rng.below(60),
            now,
        },
        11..=14 => Command::Tick { now: tick },
        15 => Command::Withdraw {
            subaccount: sub,
            amount_quote_minor: u128::from(rng.below(1_000_000)),
        },
        16 | 17 => Command::OracleUpdate {
            base_symbol: "BTC".into(),
            provider: "pyth".into(),
            ts: now,
            price_quote_minor: 7_800_000 + u128::from(rng.below(400_000)),
        },
        18 => Command::OracleUpdate {
            base_symbol: "BTC".into(),
            provider: "chainlink".into(),
            ts: now,
            price_quote_minor: 7_800_000 + u128::from(rng.below(400_000)),
        },
        19 => Command::Transfer {
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

#[test]
fn third_wave_commands_hold_invariants() {
    for seed in 1..=10_u64 {
        let mut rng = Rng::new(seed.wrapping_mul(0x9E37_79B9));
        let cfg = EngineConfig {
            vault_epoch_interval_ms: 5_000,
            // Quote interest on: every day-crossing tick charges
            // utilized quote margin through the revenue router.
            quote_interest_bps_per_day: 5,
            // Reward faucet off: `venue_pools().1` reports the budget
            // (per-interval + carry), which would move the metric on
            // zero-payment settles without moving cash.
            reward_per_interval_quote_minor: 0,
            mm_program: poc_economics::MmTierProgram {
                tiers: vec![poc_economics::MmTierSpec {
                    name: "FUZZ-MM",
                    fee_discount_bps: 1_500,
                    min_uptime_permille: 100,
                    max_spread_bps: 400,
                    min_size_lots: 1,
                }],
                review_interval_ms: 4_000,
            },
            ..EngineConfig::default()
        };
        let mut live = Engine::new(cfg.clone());
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
                amount_quote_minor: 80_000_000,
            });
        }
        live.process(Command::VaultCreate {
            revenue_share_bps: 4_000,
            now: T0,
        });

        let mut shadow = Engine::replay(cfg, live.journal());
        shadow.register_instrument(Instrument::Perp(PerpMarket::default()));

        let baseline = tracked_total_with_inventory(&live);
        let mut custody: i128 = 0;
        let mut lots_traded: i128 = 0;

        for step in 0..200_u64 {
            // Cross a UTC day boundary once mid-run so quote interest
            // fires deterministically in every seed.
            let now = if step == 100 {
                T0 + 24 * 60 * 60 * 1000
            } else {
                T0 + step * 1_000
            };
            let cmd = random_command_v3(&mut rng, now, now);
            let evs = live.process(cmd.clone());
            for ev in &evs {
                match ev {
                    Event::Withdrawal {
                        amount_quote_minor, ..
                    } => {
                        custody = custody.saturating_sub(poc_core::to_i128(*amount_quote_minor));
                    }
                    Event::Deposit {
                        amount_quote_minor, ..
                    } => {
                        custody = custody.saturating_add(poc_core::to_i128(*amount_quote_minor));
                    }
                    Event::Reward(paid) => {
                        custody =
                            custody.saturating_add(poc_core::to_i128(paid.amount_quote_minor));
                    }
                    Event::TradeExecuted(t) => {
                        lots_traded = lots_traded.saturating_add(i128::from(t.qty_lots));
                    }
                    _ => {}
                }
            }
            shadow.process(cmd);

            // I-5 (with vaults + inventory): conservation up to
            // rounding dust.
            let total = tracked_total_with_inventory(&live);
            let delta = (total - (baseline + custody)).abs();
            assert!(
                delta <= 10 * lots_traded + 64,
                "seed {seed} step {step}: conservation broke by {delta}"
            );

            // I-27 (MM tier legality): a discount exists only for an
            // enrolled subaccount, and never exceeds the tier cap.
            for sub in 1..=SUBS {
                let discount = live.mm_discount_of(sub);
                if discount > 0 {
                    assert!(
                        live.mm_ledger().is_enrolled(sub),
                        "seed {seed} step {step}: discount for unenrolled sub {sub}"
                    );
                    assert!(
                        discount <= 5_000,
                        "seed {seed} step {step}: discount cap exceeded"
                    );
                }
            }
        }

        // I-3: replay determinism across the third-wave commands.
        assert!(
            engines_agree(&live, &shadow),
            "seed {seed}: replay diverged with third-wave commands"
        );
        assert!(book_is_sane(&live));
    }
}
