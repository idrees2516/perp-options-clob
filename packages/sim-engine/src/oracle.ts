/**
 * Oracle defense — median aggregation, staleness, deviation quarantine,
 * quorum halt (poc-oracle).
 */

import type { MoneyMinor, OracleState } from "@perp/types";

/** Deviation (bps) beyond which a provider is quarantined. */
export const QUARANTINE_DEVIATION_BPS = 200;
/** Minimum healthy providers to keep a live mark. */
export const MIN_QUORUM = 2;
/** Observation considered stale after this long. */
export const STALENESS_MS = 30_000;

export interface ProviderState {
  provider: string;
  last_price_quote_minor: MoneyMinor | null;
  last_ts: number;
  quarantined: boolean;
  deviation_bps: number | null;
}

export class OracleEngine {
  readonly base_symbol: string;
  private providers = new Map<string, ProviderState>();
  private median: MoneyMinor | null = null;
  private lastUpdate = 0;
  halted = false;
  /** Emitted when a provider gets quarantined (for the journal). */
  onQuarantine: ((provider: string, price: MoneyMinor, median: MoneyMinor) => void) | null = null;
  onHalt: (() => void) | null = null;
  onResume: (() => void) | null = null;

  constructor(base: string, providers: string[]) {
    this.base_symbol = base;
    for (const p of providers) {
      this.providers.set(p, {
        provider: p,
        last_price_quote_minor: null,
        last_ts: 0,
        quarantined: false,
        deviation_bps: null,
      });
    }
  }

  observe(provider: string, price: MoneyMinor, ts: number): void {
    const st = this.providers.get(provider);
    if (!st) return;
    st.last_price_quote_minor = price;
    st.last_ts = ts;

    // Deviation quarantine: compare against the current median of healthy feeds.
    const healthy = this.healthyPrices(ts);
    if (healthy.length > 0 && this.median != null) {
      const med = Number(this.median);
      if (med > 0) {
        const devBps = Math.abs((Number(price) - med) / med) * 10_000;
        st.deviation_bps = Math.round(devBps);
        if (devBps > QUARANTINE_DEVIATION_BPS && !st.quarantined) {
          st.quarantined = true;
          this.onQuarantine?.(provider, price, this.median);
        }
      }
    } else {
      st.deviation_bps = null;
    }

    this.recompute(ts);
  }

  toggleQuarantine(provider: string): void {
    const st = this.providers.get(provider);
    if (!st) return;
    st.quarantined = !st.quarantined;
    this.recompute(Date.now()); // recompute with whatever clock the caller uses
  }

  /** Median price of healthy, fresh providers. */
  private healthyPrices(ts: number): MoneyMinor[] {
    const out: MoneyMinor[] = [];
    for (const st of this.providers.values()) {
      if (st.quarantined) continue;
      if (st.last_price_quote_minor == null) continue;
      if (ts - st.last_ts > STALENESS_MS) continue;
      out.push(st.last_price_quote_minor);
    }
    return out;
  }

  quorum(ts: number): number {
    return this.healthyPrices(ts).length;
  }

  mark(ts: number): MoneyMinor | null {
    return this.median;
  }

  private recompute(ts: number): void {
    const healthy = this.healthyPrices(ts).sort((a, b) => (a < b ? -1 : a > b ? 1 : 0));
    const wasHalted = this.halted;
    if (healthy.length < MIN_QUORUM) {
      this.halted = true;
      this.median = null;
      if (!wasHalted) this.onHalt?.();
      return;
    }
    this.halted = false;
    if (wasHalted) this.onResume?.();
    const mid = Math.floor(healthy.length / 2);
    this.median =
      healthy.length % 2 === 1 ? healthy[mid]! : (healthy[mid! - 1]! + healthy[mid]!) / 2n;
    this.lastUpdate = ts;
  }

  snapshot(ts: number): OracleState {
    return {
      base_symbol: this.base_symbol,
      providers: [...this.providers.values()].map((p) => ({
        provider: p.provider,
        base_symbol: this.base_symbol,
        last_price_quote_minor: p.last_price_quote_minor,
        last_ts: p.last_ts,
        quarantined: p.quarantined,
        deviation_bps: p.deviation_bps,
      })),
      median_quote_minor: this.median,
      quorum: this.quorum(ts),
      min_quorum: MIN_QUORUM,
      halted: this.halted,
      last_update: this.lastUpdate,
    };
  }
}
