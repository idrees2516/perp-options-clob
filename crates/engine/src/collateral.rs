//! Multi-collateral margining (G-17): non-quote balances, haircuts,
//! conversions, and the equity bridge into the portfolio margin engine.
//!
//! Design (Derive V3 unified margin, resolved for this engine):
//!
//! * Positions and PnL are margined in the **quote currency**; collateral
//!   is valued into quote at oracle prices with a per-currency **haircut**
//!   (`value = balance × price × (1 − haircut)`), rounded down — a
//!   conservative credit for the holder.
//! * The haircut value is added to **equity only** (it backs maintenance);
//!   initial-margin headroom sees the same value, so collateral cannot
//!   leverage beyond its risk-adjusted worth.
//! * Fees, funding, and realized PnL settle in quote only — a collateral
//!   currency must be converted (at oracle, zero fee) to pay them.
//! * `USD` is the reserved code for quote cash: converting `BTC → USD`
//!   increases `cash_quote_minor` and is how collateral becomes spendable.
//!
//! Determinism: balances are integer minor units per currency, prices come
//! from the same `AssetOracle` map the underlyings use (or a configured
//! fixed rate), and every mutation flows through a journaled event.

use std::collections::BTreeMap;

use poc_core::{mul_div, to_i128, SubaccountId, TimestampMs};

use crate::engine::Engine;

/// Reserved currency code for the quote cash balance.
pub const QUOTE_CODE: &str = "USD";

/// How a collateral currency is priced into quote minor units.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PriceSource {
    /// Use an underlying's oracle (e.g. BTC collateral reads the `BTC`
    /// oracle — the same feed that marks BTC-PERP).
    Oracle(String),
    /// A fixed rate: quote minor per `1.0` unit (stablecoin pegs).
    Fixed(u128),
}

/// A marginable collateral currency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollateralCurrency {
    /// Currency code (`"BTC"`, `"ETH"`, ...). `USD` is reserved.
    pub code: String,
    /// Minor units per `1.0` unit (sats → 8, wei → 18).
    pub decimals: u32,
    /// Value haircut for margin, bps (2000 = count 80% of market value).
    pub haircut_bps: u64,
    /// Price source into quote minor per `1.0` unit.
    pub price_source: PriceSource,
}

impl CollateralCurrency {
    /// A BTC-shaped default collateral: 8 decimals, 20% haircut, oracle-priced.
    #[must_use]
    pub fn btc() -> Self {
        Self {
            code: "BTC".into(),
            decimals: 8,
            haircut_bps: 2_000,
            price_source: PriceSource::Oracle("BTC".into()),
        }
    }

    /// An ETH-shaped default collateral: 18 decimals, 25% haircut.
    #[must_use]
    pub fn eth() -> Self {
        Self {
            code: "ETH".into(),
            decimals: 18,
            haircut_bps: 2_500,
            price_source: PriceSource::Oracle("ETH".into()),
        }
    }
}

impl Engine {
    /// Plan a collateral deposit (G-17).
    pub(crate) fn plan_collateral_deposit(
        &self,
        subaccount: SubaccountId,
        currency: &str,
        amount_minor: u128,
        now: TimestampMs,
    ) -> Vec<crate::event::Event> {
        let reject = |reason: &'static str| {
            vec![crate::event::Event::CollateralRejected {
                subaccount,
                currency: currency.to_owned(),
                requested_minor: amount_minor,
                reason,
                ts: now,
            }]
        };
        if amount_minor == 0 {
            return reject("amount must be positive");
        }
        if currency == QUOTE_CODE {
            // Quote deposits go through the regular deposit path.
            return vec![crate::event::Event::Deposit {
                subaccount,
                amount_quote_minor: amount_minor,
                ts: now,
            }];
        }
        if self.collateral_config(currency).is_none() {
            return reject("unknown collateral currency");
        }
        vec![crate::event::Event::CollateralMoved(Box::new(
            crate::event::CollateralMoved {
                subaccount,
                currency: currency.to_owned(),
                amount_minor: to_i128(amount_minor),
                ts: now,
            },
        ))]
    }

    /// Plan a collateral withdrawal (G-17): may not push the account into
    /// liquidation territory (equity net of the withdrawn, haircut-adjusted
    /// value must still cover maintenance).
    pub(crate) fn plan_collateral_withdraw(
        &self,
        subaccount: SubaccountId,
        currency: &str,
        amount_minor: u128,
        now: TimestampMs,
    ) -> Vec<crate::event::Event> {
        let reject = |reason: &'static str| {
            vec![crate::event::Event::CollateralRejected {
                subaccount,
                currency: currency.to_owned(),
                requested_minor: amount_minor,
                reason,
                ts: now,
            }]
        };
        if amount_minor == 0 {
            return reject("amount must be positive");
        }
        if currency == QUOTE_CODE {
            return vec![crate::event::Event::WithdrawRejected {
                subaccount,
                requested: amount_minor,
                reason: "use the quote withdrawal command",
                ts: now,
            }];
        }
        if self.collateral_config(currency).is_none() {
            return reject("unknown collateral currency");
        }
        if self.collateral_balance(subaccount, currency) < amount_minor {
            return reject("insufficient collateral balance");
        }
        // Margin gate: simulate the withdrawal on a cloned account and
        // require equity (haircut value of remaining collateral included)
        // to still cover maintenance.
        let Some(account) = self.accounts.get(&subaccount) else {
            return reject("unknown account");
        };
        let Some(price) = self.collateral_price(currency, now) else {
            return reject("collateral price unavailable");
        };
        let Some(cfg) = self.collateral_config(currency) else {
            return reject("unknown collateral currency");
        };
        let Some(unit) = 10_u128.checked_pow(cfg.decimals) else {
            return reject("collateral decimals overflow");
        };
        let gross = mul_div(amount_minor, price, unit, poc_core::Rounding::Floor).unwrap_or(0);
        let net = mul_div(
            gross,
            10_000_u128.saturating_sub(u128::from(cfg.haircut_bps)),
            10_000,
            poc_core::Rounding::Floor,
        )
        .unwrap_or(0);
        let mut summary = match self.effective_margin_summary(account) {
            Some(s) => s,
            None => return reject("account cannot be margined"),
        };
        summary.equity_quote_minor = summary.equity_quote_minor.saturating_sub(to_i128(net));
        if summary.equity_quote_minor < to_i128(summary.maintenance_quote_minor) {
            return reject("withdrawal exceeds free equity");
        }
        vec![crate::event::Event::CollateralMoved(Box::new(
            crate::event::CollateralMoved {
                subaccount,
                currency: currency.to_owned(),
                amount_minor: -to_i128(amount_minor),
                ts: now,
            },
        ))]
    }

    /// Plan a currency conversion at oracle prices (G-17). Zero fee; the
    /// margin gate keeps the account whole (converting collateral to
    /// collateral or to spendable quote never reduces equity at the same
    /// prices — haircuts only apply to the *valuation* of balances, and
    /// converting quote into a haircut asset *does* reduce marginable
    /// equity, so that direction is gated).
    pub(crate) fn plan_collateral_convert(
        &self,
        subaccount: SubaccountId,
        from: &str,
        to: &str,
        from_amount_minor: u128,
        now: TimestampMs,
    ) -> Vec<crate::event::Event> {
        use crate::event::{CollateralConverted, Event};
        let reject = |reason: &'static str| {
            vec![Event::CollateralRejected {
                subaccount,
                currency: from.to_owned(),
                requested_minor: from_amount_minor,
                reason,
                ts: now,
            }]
        };
        if from == to || from_amount_minor == 0 {
            return reject("invalid conversion");
        }
        let from_price = if from == QUOTE_CODE {
            None // quote minor is the valuation unit itself
        } else {
            match self.collateral_price(from, now) {
                Some(p) => Some((
                    p,
                    self.collateral_config(from)
                        .map(|c| c.decimals)
                        .unwrap_or(0),
                )),
                None => return reject("source price unavailable"),
            }
        };
        let to_cfg = if to == QUOTE_CODE {
            None
        } else {
            match self.collateral_config(to) {
                Some(c) => Some((c.clone(), self.collateral_price(to, now))),
                None => return reject("unknown destination currency"),
            }
        };

        // Balance check.
        let have = if from == QUOTE_CODE {
            self.accounts
                .get(&subaccount)
                .map(|a| a.cash_quote_minor.max(0) as u128)
                .unwrap_or(0)
        } else {
            self.collateral_balance(subaccount, from)
        };
        if have < from_amount_minor {
            return reject("insufficient balance");
        }

        // Valuation in quote minor: from → quote → to.
        let quote_value = match (from == QUOTE_CODE, from_price) {
            (true, _) => from_amount_minor,
            (false, Some((price, decimals))) => {
                let Some(unit) = 10_u128.checked_pow(decimals) else {
                    return reject("source decimals overflow");
                };
                mul_div(from_amount_minor, price, unit, poc_core::Rounding::Floor).unwrap_or(0)
            }
            (false, None) => return reject("source price unavailable"),
        };
        let to_amount = match (to == QUOTE_CODE, to_cfg.as_ref()) {
            (true, _) => quote_value,
            (false, Some((cfg, Some(price)))) => {
                let Some(unit) = 10_u128.checked_pow(cfg.decimals) else {
                    return reject("destination decimals overflow");
                };
                mul_div(quote_value, unit, *price, poc_core::Rounding::Floor).unwrap_or(0)
            }
            (false, _) => return reject("destination price unavailable"),
        };
        if to_amount == 0 {
            return reject("conversion rounds to zero");
        }

        // Margin gate: converting quote (full margin value) into a
        // haircut asset reduces marginable equity by the haircut —
        // refuse if that would push the account under maintenance.
        let equity_delta = if from == QUOTE_CODE {
            let haircut = to_cfg.as_ref().map_or(0, |(c, _)| c.haircut_bps);
            // to_amount is in destination minor; its quote value is
            // `quote_value`; the haircut slice is what equity loses.
            mul_div(
                quote_value,
                u128::from(haircut),
                10_000,
                poc_core::Rounding::Floor,
            )
            .unwrap_or(0)
        } else {
            0
        };
        if equity_delta > 0 {
            if let Some(account) = self.accounts.get(&subaccount) {
                if let Some(mut summary) = self.effective_margin_summary(account) {
                    summary.equity_quote_minor = summary
                        .equity_quote_minor
                        .saturating_sub(to_i128(equity_delta));
                    if summary.equity_quote_minor < to_i128(summary.maintenance_quote_minor) {
                        return reject("conversion exceeds free equity");
                    }
                }
            }
        }

        let rate = if from == QUOTE_CODE {
            1
        } else {
            match from_price {
                Some((p, _)) => p,
                None => return reject("source price unavailable"),
            }
        };
        vec![Event::CollateralConversion(Box::new(CollateralConverted {
            subaccount,
            from: from.to_owned(),
            to: to.to_owned(),
            from_amount_minor,
            to_amount_minor: to_amount,
            rate_quote_minor_per_unit: rate,
            ts: now,
        }))]
    }

    /// One configured collateral currency by code (`None` for unknown or
    /// the reserved quote code — use `cash_quote_minor` for that).
    pub(crate) fn collateral_config(&self, code: &str) -> Option<&CollateralCurrency> {
        self.config
            .collateral
            .iter()
            .find(|c| c.code == code && c.code != QUOTE_CODE)
    }

    /// Oracle price of one collateral unit in quote minor (per `1.0` unit).
    pub(crate) fn collateral_price(&self, code: &str, now: TimestampMs) -> Option<u128> {
        match &self.collateral_config(code)?.price_source {
            PriceSource::Oracle(base) => self.oracles.get(base).and_then(|o| o.mark(now)),
            PriceSource::Fixed(p) => Some(*p),
        }
    }

    /// A subaccount's raw collateral balance in one currency (0 when none).
    #[must_use]
    pub fn collateral_balance(&self, subaccount: SubaccountId, code: &str) -> u128 {
        self.collateral
            .get(&subaccount)
            .and_then(|m| m.get(code))
            .copied()
            .unwrap_or(0)
    }

    /// Haircut-adjusted collateral value of one account, quote minor.
    ///
    /// Currencies without a live price are **excluded** (conservative: an
    /// unpriceable asset backs nothing), and per-currency value rounds
    /// down at every division.
    #[must_use]
    pub fn collateral_value_of(&self, subaccount: SubaccountId, now: TimestampMs) -> u128 {
        let Some(balances) = self.collateral.get(&subaccount) else {
            return 0;
        };
        let mut total = 0_u128;
        for (code, &balance) in balances {
            if balance == 0 {
                continue;
            }
            let Some(cfg) = self.collateral_config(code) else {
                continue;
            };
            let Some(price) = self.collateral_price(code, now) else {
                continue; // unpriceable = worthless to margin
            };
            let Some(unit) = 10_u128.checked_pow(cfg.decimals) else {
                continue;
            };
            let gross = mul_div(balance, price, unit, poc_core::Rounding::Floor).unwrap_or(0);
            let net = mul_div(
                gross,
                10_000_u128.saturating_sub(u128::from(cfg.haircut_bps)),
                10_000,
                poc_core::Rounding::Floor,
            )
            .unwrap_or(0);
            total = total.saturating_add(net);
        }
        total
    }

    /// Margin summary with collateral equity included (G-17): the number
    /// every gate — withdrawals, transfers, liquidation candidacy —
    /// actually enforces. Marks are built at the engine's clock.
    #[must_use]
    pub fn effective_margin_summary(
        &self,
        account: &poc_margin::MarginAccount,
    ) -> Option<poc_margin::MarginSummary> {
        let marks = self.build_marks(self.now)?;
        self.effective_margin_summary_at(account, &marks, self.now)
    }

    /// [`Self::effective_margin_summary`] against caller-supplied marks
    /// and clock (the sweep plans at `now`, which may be ahead of the
    /// applied clock).
    #[must_use]
    pub fn effective_margin_summary_at(
        &self,
        account: &poc_margin::MarginAccount,
        marks: &BTreeMap<String, poc_margin::MarkSet>,
        now: TimestampMs,
    ) -> Option<poc_margin::MarginSummary> {
        let exposures = self.collateral_spot_exposures_of(account.id);
        let mut summary =
            self.margin_engine
                .margin_summary_ex(account, &self.instruments, marks, &exposures)?;
        let value = self.collateral_value_of(account.id, now);
        summary.equity_quote_minor = summary.equity_quote_minor.saturating_add(to_i128(value));
        Some(summary)
    }

    /// Oracle-priced collateral exposures per underlying in signed base
    /// units (G-20): the scanner treats each as a linear spot leg so
    /// held collateral hedges short optionality inside the grid.
    ///
    /// Fixed-rate collateral (stablecoins) contributes nothing — its
    /// value does not move with any underlying's spot.
    #[must_use]
    pub fn collateral_spot_exposures_of(&self, subaccount: SubaccountId) -> BTreeMap<String, f64> {
        let mut out: BTreeMap<String, f64> = BTreeMap::new();
        let Some(balances) = self.collateral.get(&subaccount) else {
            return out;
        };
        for (code, balance) in balances {
            if *balance == 0 {
                continue;
            }
            let Some(cfg) = self.collateral_config(code) else {
                continue;
            };
            let PriceSource::Oracle(base) = &cfg.price_source else {
                continue;
            };
            let base_units = *balance as f64 / 10_f64.powi(cfg.decimals as i32);
            *out.entry(base.clone()).or_insert(0.0) += base_units;
        }
        out
    }

    /// Credit or debit a collateral balance. `amount_minor` is signed.
    pub(crate) fn apply_collateral_move(
        &mut self,
        subaccount: SubaccountId,
        currency: &str,
        amount_minor: i128,
    ) {
        if currency == QUOTE_CODE {
            if let Some(account) = self.accounts.get_mut(&subaccount) {
                account.cash_quote_minor = account.cash_quote_minor.saturating_add(amount_minor);
            }
            return;
        }
        if amount_minor == 0 {
            return;
        }
        let balances = self.collateral.entry(subaccount).or_default();
        let current = balances.get(currency).copied().unwrap_or(0);
        let next = to_i128(current).saturating_add(amount_minor).max(0);
        if next == 0 {
            balances.remove(currency);
        } else {
            balances.insert(currency.to_owned(), next.unsigned_abs());
        }
    }
}
