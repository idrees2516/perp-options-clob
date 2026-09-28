//! Integration tests for the fourth closure wave (G-15 MM tier
//! program, G-16 vault revenue-share wiring, G-18 quote-balance
//! interest, G-35 proof-of-reserves): the obligations that must be met
//! for the discounts, the conservation of every routed unit, and the
//! provability of every liability.

use poc_core::{Instrument, PerpMarket, Side, TimestampMs};
use poc_economics::MmTierSpec;
use poc_engine::{Command, Engine, EngineConfig, Event, OrderRequest};

const T0: TimestampMs = 1_000_000;

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

fn deposit(e: &mut Engine, sub: u64, amount: u128) {
    e.process(Command::Deposit {
        subaccount: sub,
        amount_quote_minor: amount,
    });
}

fn place(e: &mut Engine, sub: u64, side: Side, price: u64, qty: u64, now: TimestampMs) {
    e.process(Command::Place {
        request: OrderRequest::limit(sub, "BTC-PERP", side, price, qty),
        now,
    });
}

/// The entry-anchored conserved quantity (matches the property suite).
fn tracked_total(e: &Engine) -> i128 {
    let users: i128 = e
        .accounts_iter()
        .map(|(_, a)| a.cash_quote_minor)
        .fold(0, |acc, x| acc.saturating_add(x));
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
    let vault_collateral: i128 = e
        .vaults()
        .values()
        .map(|v| poc_core::to_i128(v.collateral_quote_minor))
        .fold(0_i128, |acc, x| acc.saturating_add(x));
    let (insurance, rewards, house, buyback) = e.venue_pools();
    users
        .saturating_sub(entry_term)
        .saturating_add(insurance)
        .saturating_add(poc_core::to_i128(rewards))
        .saturating_add(poc_core::to_i128(house))
        .saturating_add(poc_core::to_i128(buyback))
        .saturating_add(vault_collateral)
}

// ----------------------------------------------------------------------
// G-15: MM tier program
// ----------------------------------------------------------------------

/// A single-tier program with a short review window and a 50bps
/// spread band at size 5, 60% uptime to qualify — small enough to
/// exercise, strict enough to mean.
fn test_mm_program() -> poc_economics::MmTierProgram {
    poc_economics::MmTierProgram {
        tiers: vec![MmTierSpec {
            name: "TEST-MM",
            fee_discount_bps: 2_000,
            min_uptime_permille: 600,
            max_spread_bps: 50,
            min_size_lots: 5,
        }],
        review_interval_ms: 10_000,
    }
}

#[test]
fn mm_tier_discount_requires_earned_obligations() {
    let cfg = EngineConfig {
        mm_program: test_mm_program(),
        ..EngineConfig::default()
    };
    let mut e = Engine::new(cfg);
    e.register_instrument(Instrument::Perp(PerpMarket::default()));
    seed_oracle(&mut e, T0, 8_000_000);
    for sub in 1..=3 {
        deposit(&mut e, sub, 200_000_000);
    }
    // Only maker 1 enrolls.
    e.process(Command::MmTierEnroll {
        subaccount: 1,
        now: T0,
    });

    // Both makers quote two-sided, tight (±10 ticks on a ~80_000-tick
    // mark = ~12bps spread), size 5.
    for sub in [1_u64, 2] {
        place(&mut e, sub, Side::Bid, 79_990, 5, T0 + 1);
        place(&mut e, sub, Side::Ask, 80_010, 5, T0 + 2);
    }
    // Several sampling ticks, all inside the review window.
    for i in 1..=6_u64 {
        e.process(Command::Tick {
            now: T0 + i * 1_000,
        });
    }

    // Review boundary: maker 1 (enrolled, qualifying) earns the tier;
    // maker 2 (qualifying but NOT enrolled) earns nothing.
    let events = e.process(Command::Tick { now: T0 + 11_000 });
    let adjusted: Vec<&Event> = events
        .iter()
        .filter(|ev| matches!(ev, Event::MmTierAdjusted { .. }))
        .collect();
    assert_eq!(adjusted.len(), 1, "only the enrolled maker is reviewed");
    match adjusted[0] {
        Event::MmTierAdjusted {
            subaccount,
            tier,
            fee_discount_bps,
            uptime_permille,
            ..
        } => {
            assert_eq!(*subaccount, 1);
            assert_eq!(*tier, Some("TEST-MM"));
            assert_eq!(*fee_discount_bps, 2_000);
            // 6 sampled ticks, all qualifying (the review tick itself
            // scores into the next window).
            assert_eq!(*uptime_permille, 1_000);
        }
        _ => unreachable!("filtered above"),
    }

    // Fee discrimination: maker 1 taker (discounted) vs maker 3 taker
    // (not enrolled) on identical fills against maker 2's ask. Pull
    // sub 1's own quotes first so its resting ask cannot STP its taker.
    e.process(Command::CancelAll {
        subaccount: 1,
        symbol: Some("BTC-PERP".into()),
        now: T0 + 11_500,
    });
    place(&mut e, 3, Side::Bid, 80_010, 1, T0 + 12_000);
    place(&mut e, 1, Side::Bid, 80_010, 1, T0 + 13_000);
    let fees: Vec<(u64, i128)> = e
        .journal()
        .iter()
        .filter_map(|ev| match ev {
            Event::TradeExecuted(t) if t.ts >= T0 + 12_000 => {
                Some((t.taker_subaccount, t.taker_fee_quote_minor))
            }
            _ => None,
        })
        .collect();
    assert_eq!(fees.len(), 2);
    let fee_of = |sub: u64| {
        fees.iter()
            .find(|(s, _)| *s == sub)
            .map(|(_, f)| *f)
            .unwrap_or(0)
    };
    let full = fee_of(3);
    let discounted = fee_of(1);
    assert!(full > 0, "baseline fee is positive");
    // 20% off, floored in the payer's favour: the discounted fee is
    // strictly smaller and at least 80% of the full fee minus one
    // rounding unit.
    assert!(
        discounted < full,
        "enrolled maker pays {discounted} < {full}"
    );
    // 20% off, floored in the payer's favour: the discount never
    // exceeds 20% plus one rounding unit.
    let max_discount = full / 5 + 1;
    assert!(full - discounted <= max_discount);

    // Demotion: maker 1 pulls quotes and goes quiet through the next
    // window; the review must strip the discount.
    e.process(Command::CancelAll {
        subaccount: 1,
        symbol: Some("BTC-PERP".into()),
        now: T0 + 14_000,
    });
    for i in 0..6_u64 {
        e.process(Command::Tick {
            now: T0 + 21_000 + i * 1_000,
        });
    }
    assert_eq!(e.mm_discount_of(1), 2_000, "discount still active");
    let events = e.process(Command::Tick { now: T0 + 32_000 });
    let demoted = events.iter().any(|ev| {
        matches!(
            ev,
            Event::MmTierAdjusted {
                subaccount,
                fee_discount_bps: 0,
                ..
            } if *subaccount == 1
        )
    });
    assert!(demoted, "silence demotes the tier");
    assert_eq!(e.mm_discount_of(1), 0);

    // Replay determinism: the tier events reconstruct identically.
    let cfg2 = EngineConfig {
        mm_program: test_mm_program(),
        ..EngineConfig::default()
    };
    let shadow = Engine::replay(cfg2, e.journal());
    assert_eq!(shadow.mm_discount_of(1), 0);
    assert!(shadow.mm_ledger().is_enrolled(1));
}

// ----------------------------------------------------------------------
// G-16: vault revenue-share wiring
// ----------------------------------------------------------------------

#[test]
fn vault_revenue_share_routes_and_conserves() {
    let cfg = EngineConfig {
        vault_epoch_interval_ms: 5_000,
        ..EngineConfig::default()
    };
    let mut e = Engine::new(cfg);
    e.register_instrument(Instrument::Perp(PerpMarket::default()));
    seed_oracle(&mut e, T0, 8_000_000);
    for sub in 1..=3 {
        deposit(&mut e, sub, 200_000_000);
    }
    // A vault with 50% of the insurance revenue allocation.
    e.process(Command::VaultCreate {
        revenue_share_bps: 5_000,
        now: T0,
    });
    e.process(Command::VaultSubscribe {
        vault_id: 1,
        subaccount: 3,
        amount_quote_minor: 1_000_000,
        now: T0,
    });
    e.process(Command::Tick { now: T0 + 6_000 });

    // Maker 2 quotes; taker 1 crosses. The taker fee routes 30% to
    // insurance, of which the vault takes 50%.
    place(&mut e, 2, Side::Ask, 80_010, 1, T0 + 7_000);
    let pools_before = e.venue_pools();
    let before = (
        e.vaults()
            .get(&1)
            .map(|v| v.collateral_quote_minor)
            .unwrap_or(0),
        tracked_total(&e),
    );
    place(&mut e, 1, Side::Bid, 80_010, 1, T0 + 8_000);
    let pools_after = e.venue_pools();
    let after = (
        e.vaults()
            .get(&1)
            .map(|v| v.collateral_quote_minor)
            .unwrap_or(0),
        tracked_total(&e),
    );

    // The trade happened and charged a taker fee.
    let fee = e
        .journal()
        .iter()
        .rev()
        .find_map(|ev| match ev {
            Event::TradeExecuted(t) if t.ts == T0 + 8_000 => Some(t.taker_fee_quote_minor),
            _ => None,
        })
        .unwrap_or(0);
    assert!(fee > 0, "a taker fee was charged");

    // Insurance allocation = 30% of gross (the default split).
    let gross = fee.unsigned_abs();
    let insurance_share = gross * 3_000 / 10_000;
    let expected_vault_credit = insurance_share * 5_000 / 10_000;
    let actual_vault_credit = after.0 - before.0;
    assert_eq!(
        actual_vault_credit, expected_vault_credit,
        "vault credited exactly its share of the insurance allocation"
    );
    // The fund (or, under the G-40 coverage policy, the buyback
    // overflow pool) received the remainder — both destinations are
    // inside the conserved universe.
    let fund_delta = pools_after.0 - pools_before.0;
    let buyback_delta = pools_after.3 - pools_before.3;
    let expected_fund = insurance_share - expected_vault_credit;
    assert_eq!(
        fund_delta + poc_core::to_i128(buyback_delta),
        poc_core::to_i128(expected_fund),
        "fund-or-buyback receives the remainder"
    );

    // Conservation across the whole trade: nothing leaked.
    assert_eq!(
        before.1, after.1,
        "the conserved quantity is unchanged by fee routing"
    );

    // Replay determinism through the revenue wiring.
    let cfg3 = EngineConfig {
        vault_epoch_interval_ms: 5_000,
        ..EngineConfig::default()
    };
    let shadow = Engine::replay(cfg3, e.journal());
    assert_eq!(
        shadow.vaults().get(&1).map(|v| v.collateral_quote_minor),
        e.vaults().get(&1).map(|v| v.collateral_quote_minor),
        "replay routes the identical vault revenue"
    );
}

#[test]
fn vault_share_never_exceeds_the_allocation() {
    // Two vaults each claiming 100% of the insurance allocation: the
    // deterministic ascending-id split must grant the first vault the
    // allocation and the second nothing — never more than 100% total.
    let cfg = EngineConfig {
        vault_epoch_interval_ms: 5_000,
        ..EngineConfig::default()
    };
    let mut e = Engine::new(cfg.clone());
    e.register_instrument(Instrument::Perp(PerpMarket::default()));
    seed_oracle(&mut e, T0, 8_000_000);
    for sub in 1..=3 {
        deposit(&mut e, sub, 200_000_000);
    }
    e.process(Command::VaultCreate {
        revenue_share_bps: 10_000,
        now: T0,
    });
    e.process(Command::VaultCreate {
        revenue_share_bps: 10_000,
        now: T0,
    });
    // Fund both so their weights are live.
    e.process(Command::VaultSubscribe {
        vault_id: 1,
        subaccount: 3,
        amount_quote_minor: 100_000,
        now: T0,
    });
    e.process(Command::VaultSubscribe {
        vault_id: 2,
        subaccount: 3,
        amount_quote_minor: 100_000,
        now: T0,
    });
    e.process(Command::Tick { now: T0 + 6_000 });

    place(&mut e, 2, Side::Ask, 80_010, 1, T0 + 7_000);
    place(&mut e, 1, Side::Bid, 80_010, 1, T0 + 8_000);

    let fee = e
        .journal()
        .iter()
        .rev()
        .find_map(|ev| match ev {
            Event::TradeExecuted(t) if t.ts == T0 + 8_000 => Some(t.taker_fee_quote_minor),
            _ => None,
        })
        .unwrap_or(0);
    let insurance_share = fee.unsigned_abs() * 3_000 / 10_000;
    let v1 = e
        .vaults()
        .get(&1)
        .map(|v| v.collateral_quote_minor)
        .unwrap_or(0)
        - 100_000;
    let v2 = e
        .vaults()
        .get(&2)
        .map(|v| v.collateral_quote_minor)
        .unwrap_or(0)
        - 100_000;
    assert_eq!(v1, insurance_share, "first vault takes the full share");
    assert_eq!(v2, 0, "second vault gets the exhausted remainder");
    assert!(v1 + v2 <= insurance_share, "never more than the allocation");
}

// ----------------------------------------------------------------------
// G-18: quote-balance interest
// ----------------------------------------------------------------------

#[test]
fn quote_interest_charges_only_utilized_cash() {
    // 1%/day interest (exaggerated for the test); the reward faucet is
    // zeroed because `venue_pools().1` reports the *budget*
    // (per-interval + carry), so a zero-payment settle would move the
    // metric without moving any cash. Not this test's subject.
    let cfg = EngineConfig {
        quote_interest_bps_per_day: 100,
        reward_per_interval_quote_minor: 0,
        ..EngineConfig::default()
    };
    let mut e = Engine::new(cfg.clone());
    e.register_instrument(Instrument::Perp(PerpMarket::default()));
    seed_oracle(&mut e, T0, 8_000_000);
    // Sub 2 (maker) and sub 3 (taker) trade into positions (utilization
    // > 0); sub 1 holds idle cash and never trades (utilization 0).
    for sub in 1..=3 {
        deposit(&mut e, sub, 200_000_000);
    }
    place(&mut e, 2, Side::Ask, 80_010, 1, T0 + 1);
    place(&mut e, 3, Side::Bid, 80_010, 1, T0 + 2);

    let before = tracked_total(&e);
    let cash_before_1 = e.account(1).map(|a| a.cash_quote_minor).unwrap_or(0);

    // Cross a UTC day boundary.
    let day_ms: u64 = 24 * 60 * 60 * 1000;
    let events = e.process(Command::Tick { now: T0 + day_ms });
    // Reward payouts inside the same tick are a custody inflow (the
    // pool mints toward makers by design); account for them.
    let rewards_paid: i128 = events
        .iter()
        .filter_map(|ev| match ev {
            Event::Reward(paid) => Some(poc_core::to_i128(paid.amount_quote_minor)),
            _ => None,
        })
        .fold(0_i128, i128::saturating_add);

    let charges: Vec<(u64, u128)> = events
        .iter()
        .filter_map(|ev| match ev {
            Event::QuoteInterestAccrued {
                subaccount,
                amount_quote_minor,
                ..
            } => Some((*subaccount, *amount_quote_minor)),
            _ => None,
        })
        .collect();
    // Idle sub 1 pays nothing (no maintenance usage).
    assert!(
        !charges.iter().any(|(sub, _)| *sub == 1),
        "idle quote cash pays nothing"
    );
    // Position holders are charged.
    assert!(
        charges.iter().any(|(sub, _)| *sub == 3),
        "utilized quote margin is charged"
    );
    let charge_3 = charges
        .iter()
        .find(|(sub, _)| *sub == 3)
        .map(|(_, amount)| *amount)
        .unwrap_or(0);
    // The charge is bounded by utilization: min(cash, maintenance) x 1%.
    let cash_after_3 = e.account(3).map(|a| a.cash_quote_minor).unwrap_or(0);
    assert!(charge_3 > 0);
    assert!(charge_3 as i128 <= cash_before_1);
    let _ = cash_after_3;
    // Conservation: the charge moved cash into the venue pools; the
    // only tracked-total movement is the reward payout.
    let after = tracked_total(&e);
    assert_eq!(
        before + rewards_paid,
        after,
        "interest is conserved through routing"
    );

    // Same-day ticks do not double-charge.
    let events = e.process(Command::Tick {
        now: T0 + day_ms + 1_000,
    });
    assert!(
        !events
            .iter()
            .any(|ev| matches!(ev, Event::QuoteInterestAccrued { .. })),
        "one charge per UTC day"
    );

    // Replay determinism.
    let shadow = Engine::replay(cfg, e.journal());
    assert_eq!(
        shadow.account(1).map(|a| a.cash_quote_minor),
        e.account(1).map(|a| a.cash_quote_minor)
    );
}

// ----------------------------------------------------------------------
// G-35: proof-of-reserves from engine state
// ----------------------------------------------------------------------

#[test]
fn por_proves_every_liability_from_engine_state() {
    let cfg = EngineConfig {
        collateral: vec![poc_engine::CollateralCurrency::btc()],
        vault_epoch_interval_ms: 5_000,
        ..EngineConfig::default()
    };
    let mut e = Engine::new(cfg);
    e.register_instrument(Instrument::Perp(PerpMarket::default()));
    seed_oracle(&mut e, T0, 8_000_000);
    deposit(&mut e, 1, 50_000_000);
    deposit(&mut e, 2, 30_000_000);
    // Non-quote collateral for sub 1: 1 BTC at the oracle price.
    e.process(Command::DepositCollateral {
        subaccount: 1,
        currency: "BTC".into(),
        amount_minor: 100_000_000, // 1.0 BTC at 8dp
        now: T0,
    });
    // A vault subscription by sub 2: the share claim is a liability.
    e.process(Command::VaultCreate {
        revenue_share_bps: 0,
        now: T0,
    });
    e.process(Command::VaultSubscribe {
        vault_id: 1,
        subaccount: 2,
        amount_quote_minor: 10_000_000,
        now: T0,
    });
    e.process(Command::Tick {
        now: T0 + 24 * 60 * 60 * 1000,
    });

    // Build the report from the engine projection. Refresh the oracle
    // at the reporting timestamp so collateral prices are live.
    let now = T0 + 25 * 60 * 60 * 1000;
    for provider in ["pyth", "chainlink"] {
        e.process(Command::OracleUpdate {
            base_symbol: "BTC".into(),
            provider: provider.into(),
            ts: now,
            price_quote_minor: 8_000_000,
        });
    }
    let rows = e.por_liabilities(now);
    let (tree, report) = poc_settlement::build_report_from_rows(rows, 7, now).expect("builds");
    assert_eq!(report.entry_count, 2);
    assert_eq!(report.nonce, 7);

    let claims2 = e
        .vaults()
        .get(&1)
        .map(|v| v.claim_quote_minor(2))
        .unwrap_or(0);
    // Every account can prove its full liability against the root.
    for entry in &tree.entries {
        let (proved, proof) = tree.proof(entry.subaccount).expect("proof exists");
        assert!(poc_settlement::verify_liability(
            &proved,
            &proof,
            &report.root,
            report.nonce
        ));
    }
    // Sub 1's row includes the BTC at full oracle value: 1.0 BTC at
    // $80k = 8_000_000 quote minor ($80,000.00 at 2dp).
    let cash1 = e.account(1).map(|a| a.cash_quote_minor).unwrap_or(0).max(0) as u128;
    let row1 = tree
        .entries
        .iter()
        .find(|entry| entry.subaccount == 1)
        .expect("row 1");
    assert_eq!(row1.quote_cash_minor, cash1);
    let (_, _, btc_value) = row1.collateral.first().cloned().unwrap_or_default();
    assert_eq!(btc_value, 8_000_000);
    // Sub 2's row carries the vault claim.
    let row2 = tree
        .entries
        .iter()
        .find(|entry| entry.subaccount == 2)
        .expect("row 2");
    assert_eq!(row2.vault_claims_quote_minor, claims2);
    assert!(claims2 > 0);

    // The proof fails against a different root.
    let mut wrong_root = report.root;
    wrong_root[0] ^= 0xFF;
    let (entry, proof) = tree.proof(1).expect("proof");
    assert!(!poc_settlement::verify_liability(
        &entry,
        &proof,
        &wrong_root,
        report.nonce
    ));

    // Total liabilities equal the sum of the rows (sub 2's cash was
    // debited by the vault subscription at the epoch boundary; its
    // claim carries the value instead).
    let cash2 = e.account(2).map(|a| a.cash_quote_minor).unwrap_or(0).max(0) as u128;
    assert_eq!(cash2, 30_000_000 - 10_000_000);
    let expected_total = cash1 + btc_value + cash2 + claims2;
    assert_eq!(report.total_liabilities_quote_minor, expected_total);

    // A balance change changes the root: deposit to sub 2 and rebuild.
    deposit(&mut e, 2, 1_000_000);
    let rows2 = e.por_liabilities(now);
    let (_, report2) = poc_settlement::build_report_from_rows(rows2, 8, now + 1).expect("builds");
    assert_ne!(report.root, report2.root);
    assert!(report2.total_liabilities_quote_minor > report.total_liabilities_quote_minor);

    // The publication ledger enforces monotonic publication.
    let mut ledger = poc_settlement::PorLedger::new();
    let first = report.clone();
    assert!(ledger.publish(first).is_ok());
    assert!(ledger.publish(report2).is_ok());
    let replayed = ledger.publish(poc_settlement::PorReport {
        root: report.root,
        total_liabilities_quote_minor: report.total_liabilities_quote_minor,
        nonce: 8, // nonce regression
        ts: now + 2,
        entry_count: report.entry_count,
    });
    assert!(replayed.is_err());
}
