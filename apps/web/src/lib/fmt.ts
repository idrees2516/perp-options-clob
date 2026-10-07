"use client";

/**
 * Display formatters — every number in the terminal goes through here.
 * Convention: USD money from quote minor units; sizes in base units.
 */

import type { Instrument, MarketRow, MoneyMinor } from "@perp/types";
import { formatMoney } from "@perp/types";

export function usd(minor: bigint | null | undefined, opts?: { sign?: boolean; compact?: boolean }): string {
  if (minor == null) return "—";
  const s = formatMoney(minor, 2);
  const neg = s.startsWith("-");
  const body = neg ? s.slice(1) : s;
  const out = opts?.compact ? `$${compactNum(minor)}` : `$${body}`;
  if (opts?.sign && !neg && minor !== 0n) return `+${out}`;
  return neg ? `−${out}` : out;
}

export function usdCompact(minor: bigint | null | undefined): string {
  if (minor == null) return "—";
  return `$${compactNum(minor)}`;
}

function compactNum(minor: bigint): string {
  const units = Number(minor) / 100;
  const abs = Math.abs(units);
  if (abs >= 1_000_000_000) return `${(units / 1e9).toFixed(2)}B`;
  if (abs >= 1_000_000) return `${(units / 1e6).toFixed(2)}M`;
  if (abs >= 10_000) return `${(units / 1e3).toFixed(1)}K`;
  return units.toLocaleString("en-US", { maximumFractionDigits: 2 });
}

/** Signed PnL with explicit sign and color semantics upstream. */
export function pnl(minor: bigint | null | undefined): string {
  if (minor == null) return "—";
  if (minor === 0n) return "$0.00";
  return usd(minor, { sign: true });
}

/** Price in quote units (per base) from minor. */
export function priceQuote(minor: bigint | null | undefined, decimals = 2): string {
  if (minor == null) return "—";
  // formatMoney already groups thousands — never re-parse through Number().
  return formatMoney(minor, decimals);
}

/** Price from ticks using the instrument tick size. */
export function ticksToPrice(ticks: number | null | undefined, inst: Instrument | null | undefined): string {
  if (ticks == null || !inst) return "—";
  return priceQuote(inst.tick_size_quote_minor * BigInt(ticks), inst.kind === "option" ? 2 : 2);
}

/** Base size (BTC) from lots. */
export function sizeBase(lots: number | null | undefined, inst: Instrument | null | undefined): string {
  if (lots == null || !inst) return "—";
  const base = BigInt(lots) * inst.lot_size_base_minor;
  const decimals = inst.base_decimals;
  const s = formatMoney(base, decimals);
  return s;
}

/** Percent change from session open. */
export function changePct(row: MarketRow): number | null {
  if (row.open_quote_minor == null || row.mark_quote_minor == null || row.open_quote_minor === 0n) return null;
  return Number((row.mark_quote_minor - row.open_quote_minor) * 10_000n / row.open_quote_minor) / 100;
}

export function pct(x: number | null | undefined, digits = 2): string {
  if (x == null) return "—";
  return `${x > 0 ? "+" : ""}${x.toFixed(digits)}%`;
}

export function bps(x: number | null | undefined): string {
  if (x == null) return "—";
  return `${x > 0 ? "+" : ""}${x.toFixed(1)}bp`;
}

/** IV display: 0.55 → 55.0% */
export function iv(x: number | null | undefined): string {
  if (x == null) return "—";
  return `${(x * 100).toFixed(1)}%`;
}

/** Time — sim clock renders as HH:MM:SS UTC of the engine clock. */
export function simTime(ts: number | null | undefined): string {
  if (ts == null) return "—";
  const d = new Date(ts);
  return `${String(d.getUTCHours()).padStart(2, "0")}:${String(d.getUTCMinutes()).padStart(2, "0")}:${String(d.getUTCSeconds()).padStart(2, "0")}`;
}

export function simDate(ts: number | null | undefined): string {
  if (ts == null) return "—";
  return new Date(ts).toLocaleDateString("en-US", { month: "short", day: "numeric", timeZone: "UTC" });
}

export function relTime(ts: number | null | undefined, now: number): string {
  if (ts == null) return "—";
  const delta = ts - now;
  const abs = Math.abs(delta);
  const s = Math.round(abs / 1000);
  const m = Math.round(s / 60);
  const h = Math.round(m / 60);
  const d = Math.round(h / 24);
  let out: string;
  if (d >= 1) out = `${d}d`;
  else if (h >= 1) out = `${h}h`;
  else if (m >= 1) out = `${m}m`;
  else out = `${s}s`;
  return delta >= 0 ? `in ${out}` : `${out} ago`;
}

/** Short symbol label for dense tables. */
export function shortSymbol(symbol: string): string {
  const m = /^(.+)-(EVER|\d{8})-(\d+)-([CP])$/.exec(symbol);
  if (!m) return symbol;
  const [, base, expiry, strike, cp] = m;
  if (expiry === "EVER") return `${base} ${Number(strike).toLocaleString()}${cp === "C" ? "C" : "P"} ∞`;
  const d = new Date(
    Number(expiry.slice(0, 4)),
    Number(expiry.slice(4, 6)) - 1,
    Number(expiry.slice(6, 8)),
  );
  const label = d.toLocaleDateString("en-US", { month: "short", day: "numeric" });
  return `${label} ${Number(strike).toLocaleString()}${cp === "C" ? "C" : "P"}`;
}

/** Health → semantic color class. */
export function healthClass(h: string | null | undefined): string {
  switch (h) {
    case "healthy":
      return "text-up";
    case "restricted":
      return "text-amber-400";
    case "liquidation":
      return "text-down";
    default:
      return "text-muted-foreground";
  }
}

/** Side → color class. */
export function sideClass(side: string): string {
  return side === "bid" ? "text-up" : "text-down";
}

export function healthLabel(h: string | null | undefined): string {
  if (!h) return "—";
  return h.charAt(0).toUpperCase() + h.slice(1);
}

export type { MoneyMinor };
