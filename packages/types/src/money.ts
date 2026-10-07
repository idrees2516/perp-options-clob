/**
 * Money helpers — exact arithmetic over bigint quote minor units.
 * Mirrors the Rust rule: `f64` never touches a ledger.
 */

import type { MoneyMinor, SignedMoneyMinor } from "./primitives";

/** Quote decimals of the venue (2 → cents). */
export const QUOTE_DECIMALS = 2n;

export const ONE_QUOTE_MINOR: MoneyMinor = 100n; // 1_00

/** 10^18 style scaling for u128 mul/div with BigInt is native-precision. */
export function mulDiv(amount: bigint, mul: bigint, div: bigint): bigint {
  if (div === 0n) throw new Error("division by zero");
  return (amount * mul) / div;
}

/** Basis-points of a minor-unit amount, rounding down (conservative for credits). */
export function bpsOf(amount: bigint, bps: number): bigint {
  return (amount * BigInt(bps)) / 10_000n;
}

/** Basis-points of a minor-unit amount, rounding up (conservative for charges). */
export function bpsOfUp(amount: bigint, bps: number): bigint {
  const div = 10_000n;
  const prod = amount * BigInt(bps);
  const q = prod / div;
  return prod % div === 0n ? q : q + 1n;
}

export function absMinor(x: SignedMoneyMinor): MoneyMinor {
  return x < 0n ? -x : x;
}

export function sign(x: SignedMoneyMinor): -1 | 0 | 1 {
  return x < 0n ? -1 : x > 0n ? 1 : 0;
}

export function minMinor(a: bigint, b: bigint): bigint {
  return a < b ? a : b;
}

export function maxMinor(a: bigint, b: bigint): bigint {
  return a > b ? a : b;
}

export function clampMinor(x: bigint, lo: bigint, hi: bigint): bigint {
  return minMinor(maxMinor(x, lo), hi);
}

/**
 * Format minor units to a display string with grouping.
 * @param decimals asset decimals (quote: 2)
 */
export function formatMoney(amount: bigint, decimals = 2, opts: Intl.NumberFormatOptions = {}): string {
  const neg = amount < 0n;
  const abs = neg ? -amount : amount;
  const scale = 10n ** BigInt(decimals);
  const whole = abs / scale;
  const frac = abs % scale;
  const wholeStr = whole.toLocaleString("en-US");
  const fracStr = frac.toString().padStart(Number(decimals), "0");
  const base = decimals > 0 ? `${wholeStr}.${fracStr}` : wholeStr;
  return neg ? `-${base}` : base;
}

/** Compact notional display: $1.23M, $456.7K. */
export function formatMoneyCompact(amount: bigint, decimals = 2): string {
  const neg = amount < 0n;
  const abs = neg ? -amount : amount;
  const scale = 10n ** BigInt(decimals);
  const units = Number(abs) / Number(scale);
  let out: string;
  if (Math.abs(units) >= 1_000_000_000) out = `${(units / 1e9).toFixed(2)}B`;
  else if (Math.abs(units) >= 1_000_000) out = `${(units / 1e6).toFixed(2)}M`;
  else if (Math.abs(units) >= 1_000) out = `${(units / 1e3).toFixed(1)}K`;
  else out = units.toFixed(2);
  return neg ? `-$${out}` : `$${out}`;
}

/** Signed-percentage for rates: renders +12.5bps / -3.2bps. */
export function formatBps(bps: number): string {
  const s = bps > 0 ? "+" : "";
  return `${s}${bps.toFixed(1)}bps`;
}

/** Permille → percentage display (e.g. 987‰ → 98.7%). */
export function formatPermille(pm: number): string {
  return `${(pm / 10).toFixed(1)}%`;
}

/** Parse a user-typed decimal string into minor units. Throws on junk. */
export function parseMoney(input: string, decimals = 2): bigint {
  const trimmed = input.trim();
  if (!/^-?\d*(\.\d*)?$/.test(trimmed) || trimmed === "" || trimmed === "-" || trimmed === ".")
    throw new Error("invalid amount");
  const neg = trimmed.startsWith("-");
  const body = neg ? trimmed.slice(1) : trimmed;
  const [wholeStr, fracStrRaw = ""] = body.split(".");
  const fracStr = (fracStrRaw + "0".repeat(Number(decimals))).slice(0, Number(decimals));
  const value = BigInt(wholeStr || "0") * 10n ** BigInt(decimals) + BigInt(fracStr || "0");
  return neg ? -value : value;
}

/** Safe parse returning null instead of throwing. */
export function tryParseMoney(input: string, decimals = 2): bigint | null {
  try {
    return parseMoney(input, decimals);
  } catch {
    return null;
  }
}

/** Ticks → minor units. */
export function ticksToMinor(ticks: number, tickSizeMinor: bigint): bigint {
  return BigInt(ticks) * tickSizeMinor;
}

/** Minor units → ticks (must divide exactly for price levels). */
export function minorToTicks(minor: bigint, tickSizeMinor: bigint): number {
  if (tickSizeMinor === 0n) return 0;
  return Number(minor / tickSizeMinor);
}

/** Lots → base minor units. */
export function lotsToBaseMinor(lots: number, lotSizeBaseMinor: bigint): bigint {
  return BigInt(lots) * lotSizeBaseMinor;
}

/** Format lots as base size (e.g. 0.001 BTC) using base decimals. */
export function formatLots(lots: number, lotSizeBaseMinor: bigint, baseDecimals: number): string {
  const baseMinor = lotsToBaseMinor(lots, lotSizeBaseMinor);
  return formatMoney(baseMinor, baseDecimals, { maximumFractionDigits: baseDecimals });
}
