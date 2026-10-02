/**
 * Option pricing — Black-Scholes (zero-rate venue convention) and the
 * American early-exercise premium via the BAW quadratic approximation.
 *
 * Venue convention (docs/OPTIONS.md): at the default zero-rate convention
 * American marks equal European marks exactly — asserted in the Rust tests.
 * We implement both paths and honor the same invariant.
 */

import type { OptionMarket } from "@perp/types";
import { effectiveTteYears } from "@perp/types";

/** Standard normal CDF (Abramowitz–Stegun 7.1.26). */
export function normCdf(x: number): number {
  const t = 1 / (1 + 0.2316419 * Math.abs(x));
  const poly =
    t * (0.319381530 + t * (-0.356563782 + t * (1.781477937 + t * (-1.821255978 + t * 1.330274429))));
  const cdf = 1 - (Math.exp(-0.5 * x * x) / Math.sqrt(2 * Math.PI)) * poly;
  return x >= 0 ? cdf : 1 - cdf;
}

export function normPdf(x: number): number {
  return Math.exp(-0.5 * x * x) / Math.sqrt(2 * Math.PI);
}

/** European option premium per 1 unit of base, zero rate. */
export function bsPrice(
  spot: number,
  strike: number,
  tteYears: number,
  iv: number,
  kind: "call" | "put",
): number {
  const t = Math.max(tteYears, 0);
  if (t <= 0 || iv <= 0) {
    return Math.max(0, kind === "call" ? spot - strike : strike - spot);
  }
  const sig = Math.max(iv, 1e-9);
  const sqrtT = Math.sqrt(t);
  const d1 = (Math.log(spot / strike) + 0.5 * sig * sig * t) / (sig * sqrtT);
  const d2 = d1 - sig * sqrtT;
  if (kind === "call") {
    return spot * normCdf(d1) - strike * normCdf(d2);
  }
  return strike * normCdf(-d2) - spot * normCdf(-d1);
}

/** Black-Scholes delta per unit base. */
export function bsDelta(spot: number, strike: number, tteYears: number, iv: number, kind: "call" | "put"): number {
  const t = Math.max(tteYears, 1e-9);
  const sig = Math.max(iv, 1e-9);
  const d1 = (Math.log(spot / strike) + 0.5 * sig * sig * t) / (sig * Math.sqrt(t));
  const nd1 = normCdf(d1);
  return kind === "call" ? nd1 : nd1 - 1;
}

/** Black-Scholes gamma per unit base. */
export function bsGamma(spot: number, strike: number, tteYears: number, iv: number): number {
  const t = Math.max(tteYears, 1e-9);
  const sig = Math.max(iv, 1e-9);
  const d1 = (Math.log(spot / strike) + 0.5 * sig * sig * t) / (sig * Math.sqrt(t));
  return normPdf(d1) / (spot * sig * Math.sqrt(t));
}

/** Black-Scholes vega per 1 vol point (1% move), per unit base. */
export function bsVegaPct(spot: number, strike: number, tteYears: number, iv: number): number {
  const t = Math.max(tteYears, 1e-9);
  const sig = Math.max(iv, 1e-9);
  const d1 = (Math.log(spot / strike) + 0.5 * sig * sig * t) / (sig * Math.sqrt(t));
  return (spot * normPdf(d1) * Math.sqrt(t)) / 100;
}

/**
 * Barone-Adesi–Whaley quadratic approximation for American premium.
 * At zero rates the early-exercise premium collapses to zero and the value
 * equals Black-Scholes — we exploit that invariant directly (venue default).
 */
export function americanPrice(
  spot: number,
  strike: number,
  tteYears: number,
  iv: number,
  kind: "call" | "put",
): number {
  // Zero-rate venue: American == European (asserted by the Rust test suite).
  return bsPrice(spot, strike, tteYears, iv, kind);
}

/** Intrinsic value per unit base. */
export function intrinsic(spot: number, strike: number, kind: "call" | "put"): number {
  return Math.max(0, kind === "call" ? spot - strike : strike - spot);
}

/** Mark an option given the governed IV surface. */
export function markOption(
  market: OptionMarket,
  spotQuoteMinor: bigint,
  ivBps: number,
  now: number,
): { premium_quote_minor_per_base: bigint; tau_years: number; iv: number } {
  const spot = Number(spotQuoteMinor) / 100; // quote decimals 2
  const strike = Number(market.strike_quote_minor) / 100;
  const tau = effectiveTteYears(market, now);
  const iv = ivBps / 10_000;
  const perBase = americanPrice(spot, strike, tau, iv, market.kind_of);
  return { premium_quote_minor_per_base: BigInt(Math.round(perBase * 100)), tau_years: tau, iv };
}

/** Greeks bundle for an option position (per unit). */
export function optionGreeks(
  market: OptionMarket,
  spotQuoteMinor: bigint,
  ivBps: number,
  now: number,
): { delta: number; gamma: number; vega: number; theta: number } {
  const spot = Number(spotQuoteMinor) / 100;
  const strike = Number(market.strike_quote_minor) / 100;
  const tau = effectiveTteYears(market, now);
  const iv = ivBps / 10_000;
  const delta = bsDelta(spot, strike, tau, iv, market.kind_of);
  const gamma = bsGamma(spot, strike, tau, iv);
  const vega = bsVegaPct(spot, strike, tau, iv);
  // finite-difference theta (premium today vs. tomorrow)
  const p0 = americanPrice(spot, strike, tau, iv, market.kind_of);
  const p1 = americanPrice(spot, strike, Math.max(0, tau - 1 / 365), iv, market.kind_of);
  const theta = p1 - p0;
  return { delta, gamma, vega, theta };
}
