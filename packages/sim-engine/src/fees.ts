/**
 * Fees, revenue routing, and tier accounting — poc-economics economics.
 */

import type { FeeTier, MoneyMinor, RevenueLedger } from "@perp/types";
import { FEE_TIERS, OPTION_FEE_CAPS, REVENUE_SPLIT } from "@perp/types";
import { bpsOfUp } from "@perp/types";

export function feeTierFor(trailingVolumeQuoteMinor: bigint): FeeTier {
  let tier = FEE_TIERS[0]!;
  for (const t of FEE_TIERS) {
    if (trailingVolumeQuoteMinor >= t.volume_threshold_quote_minor) tier = t;
  }
  return tier;
}

/** Effective bps after MM tier discount (bounded at 50%, composes after the ladder). */
export function applyMmDiscount(bps: number, mmDiscountBps: number): number {
  const discount = Math.min(mmDiscountBps, 5000) / 10_000;
  const effective = bps * (1 - discount);
  return bps < 0 ? effective : Math.max(0, effective);
}

export interface FeeQuote {
  fee_quote_minor: bigint; // signed: negative = rebate
  capped: boolean;
}

/**
 * Compute a fill fee. Options cap at a fraction of premium (F-2):
 * taker ≤ 12.5% of premium, maker ≤ 2.5%.
 */
export function computeFillFee(
  kind: "taker" | "maker",
  isOption: boolean,
  notionalQuoteMinor: bigint,
  premiumQuoteMinor: bigint | null,
  tier: FeeTier,
  mmDiscountBps: number,
): FeeQuote {
  const rawBps = kind === "taker" ? tier.taker_bps : tier.maker_bps;
  const bps = applyMmDiscount(rawBps, mmDiscountBps);
  let fee = bps >= 0 ? bpsOfUp(notionalQuoteMinor, Math.round(bps)) : -bpsOfUp(notionalQuoteMinor, Math.round(-bps));
  let capped = false;
  if (isOption && premiumQuoteMinor != null && premiumQuoteMinor > 0n) {
    const capBps = kind === "taker" ? OPTION_FEE_CAPS.taker_cap_bps : OPTION_FEE_CAPS.maker_cap_bps;
    const cap = bpsOfUp(premiumQuoteMinor, capBps);
    if (fee > cap) {
      fee = cap;
      capped = true;
    }
  }
  return { fee_quote_minor: fee, capped };
}

export function newRevenueLedger(): RevenueLedger {
  return {
    house_quote_minor: 0n,
    insurance_quote_minor: 0n,
    buyback_quote_minor: 0n,
    insurance_overflow_quote_minor: 0n,
  };
}

/**
 * Route net fee income through the 60/30/10 router. The insurance share
 * flows through LP vaults first (G-16); overflow above the coverage target
 * spills to the buyback pool (G-40).
 */
export function routeFee(
  ledger: RevenueLedger,
  feeQuoteMinor: bigint,
  vaults: { takeFeeAllocation: (allocation: bigint) => bigint },
  coveragePermille: number,
  coverageTargetPermille: number,
): void {
  if (feeQuoteMinor <= 0n) return;
  const house = bpsOfUp(feeQuoteMinor, REVENUE_SPLIT.house_bps);
  let insurance = bpsOfUp(feeQuoteMinor, REVENUE_SPLIT.insurance_bps);
  let buyback = feeQuoteMinor - house - insurance; // remainder (rounding-consistent)

  // Vault underwriters take their configured bps of the insurance allocation.
  const vaultTake = vaults.takeFeeAllocation(insurance);
  insurance -= vaultTake;

  // Above target coverage, the insurance share overflows to buyback.
  if (coveragePermille >= coverageTargetPermille) {
    ledger.insurance_overflow_quote_minor += insurance;
    buyback += insurance;
    insurance = 0n;
  }

  ledger.house_quote_minor += house;
  ledger.insurance_quote_minor += insurance;
  ledger.buyback_quote_minor += buyback;
}

/** Day bucket key for the trailing volume ledger. */
export function dayKey(ts: number): string {
  return new Date(ts).toISOString().slice(0, 10);
}

/** Prune buckets older than 30 days and total the trailing window. */
export function trailingVolume(buckets: Map<string, bigint>, now: number): bigint {
  const cutoff = now - 30 * 24 * 3600 * 1000;
  let total = 0n;
  for (const [k, v] of buckets) {
    if (new Date(k + "T00:00:00Z").getTime() < cutoff) buckets.delete(k);
    else total += v;
  }
  return total;
}
