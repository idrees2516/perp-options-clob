/**
 * Snapshot builder — projects the engine state into the UI view-model
 * (VenueSnapshot), including the proof-of-reserves merkle report.
 */

import type {
  AccountView,
  BookView,
  CollateralBalance,
  FillRow,
  InsuranceState,
  MarketRow,
  OpenOrderRow,
  PorLiabilityRow,
  PorReport,
  PositionRow,
  SubaccountId,
  Symbol,
  VenueSnapshot,
} from "@perp/types";
import { ACCOUNT_DESCRIPTORS, type SimEngine } from "./engine";
import { buildMerkle, proveInclusion } from "./merkle";
import { sha256Tagged } from "./sha256";

export function buildMarketRows(e: SimEngine): MarketRow[] {
  const rows: MarketRow[] = [];
  for (const [sym, inst] of e.instruments) {
    const book = e.books.get(sym);
    const bb = book?.bestBid() ?? null;
    const ba = book?.bestAsk() ?? null;
    const mark = e.markOf(sym);
    const ring = e.volume24.get(sym) ?? [];
    const cutoff = e.now - 24 * 3600 * 1000;
    let volume = 0n;
    for (const x of ring) if (x.ts >= cutoff) volume += x.notional;
    const open = e.sessionOpen.get(sym);
    const isOption = inst.kind === "option";
    rows.push({
      symbol: sym,
      kind: isOption ? "option" : "perp",
      base_symbol: inst.base_symbol,
      label: marketLabel(sym, inst.kind),
      mark_quote_minor: mark,
      best_bid_ticks: bb,
      best_ask_ticks: ba,
      spread_ticks: bb != null && ba != null ? ba - bb : null,
      volume_quote_minor: volume,
      open_quote_minor: open ?? null,
      iv: isOption ? (e.ivs.get(sym) ?? 5500) / 10_000 : null,
      funding_rate_bps: !isOption ? (e.fundingTrackers.get(sym)?.last_rate_bps ?? null) : (e.everlastingRoll.get(sym)?.last_rate_bps ?? null),
      expired: false,
      halted: e.breakers.has(sym),
      variant: isOption ? inst.variant : null,
      exercise_style: isOption ? inst.exercise_style : null,
      strike_quote_minor: isOption ? inst.strike_quote_minor : null,
      expiry_ts: isOption ? (inst.variant === "dated" ? inst.expiry_ts_ms : null) : null,
    });
  }
  return rows;
}

function marketLabel(sym: string, kind: "perp" | "option"): string {
  if (kind === "perp") return sym;
  const m = /^(.+)-(EVER|\d{8})-(\d+)-([CP])$/.exec(sym);
  if (!m) return sym;
  const [, base, expiry, strike, cp] = m;
  const style = cp === "C" ? "Call" : "Put";
  if (expiry === "EVER") return `${base} EVER ${(Number(strike) / 1).toLocaleString()} ${style}`;
  const y = Number(expiry.slice(0, 4));
  const mo = Number(expiry.slice(4, 6));
  const d = Number(expiry.slice(6, 8));
  const date = new Date(Date.UTC(y, mo - 1, d));
  const label = date.toLocaleDateString("en-US", { month: "short", day: "numeric", timeZone: "UTC" });
  return `${label} ${(Number(strike) / 1).toLocaleString()} ${style}`;
}

export function buildAccountViews(e: SimEngine): AccountView[] {
  const views: AccountView[] = [];
  for (const d of ACCOUNT_DESCRIPTORS) {
    const acc = e.accounts.get(d.id);
    if (!acc) continue;
    const m = e.marginOf(d.id);
    views.push({
      id: d.id,
      summary: {
        equity_quote_minor: m.equity,
        maintenance_quote_minor: m.maintenance,
        initial_quote_minor: m.initial,
        order_margin_quote_minor: m.order_margin,
      },
      cash_quote_minor: acc.cash_quote_minor,
      positions: [...acc.positions.values()].map((p) => ({
        symbol: p.symbol,
        signed_lots: p.signed_lots,
        avg_entry_quote_minor: p.avg_entry_quote_minor,
      })),
      fees_paid_quote_minor: acc.fees_paid_quote_minor,
      funding_received_quote_minor: acc.funding_pnl_quote_minor,
      open_order_ids: [...acc.open_orders.keys()],
      health: m.health,
    });
  }
  return views;
}

export function buildPositionRows(e: SimEngine, sub: SubaccountId): PositionRow[] {
  const acc = e.accounts.get(sub);
  if (!acc) return [];
  const rows: PositionRow[] = [];
  for (const [sym, pos] of acc.positions) {
    const inst = e.instruments.get(sym);
    if (!inst) continue;
    const mark = e.markOf(sym);
    const scale = 10n ** BigInt(inst.base_decimals);
    const notional = (BigInt(Math.abs(pos.signed_lots)) * inst.lot_size_base_minor * mark) / scale;
    const upnl = (BigInt(pos.signed_lots) * inst.lot_size_base_minor * (mark - pos.avg_entry_quote_minor)) / scale;
    rows.push({
      symbol: sym,
      kind: inst.kind === "option" ? "option" : "perp",
      signed_lots: pos.signed_lots,
      avg_entry_quote_minor: pos.avg_entry_quote_minor,
      mark_quote_minor_per_base: mark,
      unrealized_pnl_quote_minor: upnl,
      realized_pnl_quote_minor: pos.realized_pnl_quote_minor,
      notional_quote_minor: notional,
    });
  }
  return rows;
}

export function buildOpenOrderRows(e: SimEngine, sub: SubaccountId): OpenOrderRow[] {
  const acc = e.accounts.get(sub);
  if (!acc) return [];
  const rows: OpenOrderRow[] = [];
  for (const [, info] of acc.open_orders) {
    const o = e.orders.get(info.order_id);
    rows.push({
      order_id: info.order_id,
      symbol: info.symbol,
      side: info.side,
      order_type_label: orderTypeLabel(o?.order_type ?? { kind: "limit" }),
      price_ticks: info.price_ticks,
      qty_lots: o?.qty_lots ?? info.open_lots,
      filled_lots: (o?.qty_lots ?? info.open_lots) - info.open_lots,
      open_lots: info.open_lots,
      tif_label: tifLabel(o?.tif ?? { kind: "gtc" }),
      post_only: o?.post_only ?? false,
      reduce_only: o?.reduce_only ?? false,
      ts: o?.engine_ts ?? e.now,
      oco_group: o?.oco_group ?? null,
    });
  }
  // Include pending stops so users see their parked stop orders.
  for (const [id, stop] of e.pendingStops) {
    if (stop.order.subaccount !== sub) continue;
    rows.push({
      order_id: id,
      symbol: stop.order.symbol,
      side: stop.order.side,
      order_type_label: orderTypeLabel(stop.order.order_type),
      price_ticks: stop.order.price_ticks,
      qty_lots: stop.order.qty_lots,
      filled_lots: 0,
      open_lots: stop.order.qty_lots,
      tif_label: tifLabel(stop.order.tif),
      post_only: stop.order.post_only,
      reduce_only: stop.order.reduce_only,
      ts: e.now,
      oco_group: stop.order.oco_group,
    });
  }
  return rows;
}

function orderTypeLabel(ot: import("@perp/types").OrderType): string {
  switch (ot.kind) {
    case "limit": return "Limit";
    case "market": return "Market";
    case "stop_market": return "Stop Mkt";
    case "stop_limit": return "Stop Lmt";
    case "trailing_stop_market": return "Trail Mkt";
    case "trailing_stop_limit": return "Trail Lmt";
  }
}

function tifLabel(tif: import("@perp/types").TimeInForce): string {
  switch (tif.kind) {
    case "gtc": return "GTC";
    case "ioc": return "IOC";
    case "fok": return "FOK";
    case "gtd": return "GTD";
  }
}

export function buildFills(e: SimEngine, limit = 300): FillRow[] {
  const out: FillRow[] = [];
  const journalTail = e.journal.slice(-limit * 2);
  for (const entry of journalTail) {
    const ev = entry.event;
    if (ev.type !== "trade_executed") continue;
    const t = ev.payload;
    out.push({
      seq: t.seq,
      ts: t.ts,
      symbol: t.symbol,
      price_ticks: t.price_ticks,
      qty_lots: t.qty_lots,
      maker_side: t.maker_side,
      notional_quote_minor: t.notional_quote_minor,
      subaccount: t.taker_subaccount,
      role: "taker",
      fee_quote_minor: t.taker_fee_quote_minor,
      liquidity_label: "T",
    });
    out.push({
      seq: t.seq,
      ts: t.ts,
      symbol: t.symbol,
      price_ticks: t.price_ticks,
      qty_lots: t.qty_lots,
      maker_side: t.maker_side,
      notional_quote_minor: t.notional_quote_minor,
      subaccount: t.maker_subaccount,
      role: "maker",
      fee_quote_minor: t.maker_fee_quote_minor,
      liquidity_label: "M",
    });
  }
  return out.slice(-limit);
}

export function buildBookViews(e: SimEngine): BookView[] {
  const books: BookView[] = [];
  for (const [sym, book] of e.books) {
    const inst = e.instruments.get(sym);
    if (!inst) continue;
    books.push({
      symbol: sym,
      best_bid_ticks: book.bestBid(),
      best_ask_ticks: book.bestAsk(),
      open_orders: book.openOrderCount(),
      halted: e.breakers.has(sym),
      kind: inst.kind === "option" ? "option" : "perp",
    });
  }
  return books;
}

export function buildInsurance(e: SimEngine): InsuranceState {
  const inventory = [...e.insurance_inventory.entries()]
    .filter(([, lots]) => lots !== 0)
    .map(([symbol, lots]) => ({ symbol, signed_lots: lots, mark_quote_minor: e.markOf(symbol) }));
  return {
    balance_quote_minor: e.insurance_balance,
    inventory,
    coverage_permille: e.coveragePermille(),
    liquidations: e.stats.liquidations,
    adls: e.stats.adls,
    total_absorbed_quote_minor: e.insurance_absorbed_total,
  };
}

export function buildCollateral(e: SimEngine): Record<SubaccountId, CollateralBalance[]> {
  const out: Record<SubaccountId, CollateralBalance[]> = {};
  for (const [id, acc] of e.accounts) {
    const rows: CollateralBalance[] = [];
    for (const [code, bal] of acc.collateral) {
      const value = code === "BTC" ? (bal * e.spot_quote_minor * 80n) / 100n / 10n ** 8n : bal;
      rows.push({ currency: code, balance_minor: bal, value_quote_minor: value });
    }
    out[id] = rows;
  }
  return out;
}

/* ═════════════════════════ proof of reserves ═════════════════════════ */

export function buildPor(e: SimEngine): { report: PorReport; rows: PorLiabilityRow[] } {
  e.porNonceBump();
  const nonce = e.porNonceGet();
  const rows: PorLiabilityRow[] = [];
  const leaves: string[] = [];
  const payloads: string[] = [];
  for (const d of ACCOUNT_DESCRIPTORS) {
    const acc = e.accounts.get(d.id);
    if (!acc) continue;
    const collateral = [...acc.collateral.entries()].map(([code, bal]) => ({
      code,
      balance_minor: bal,
      value_quote_minor: code === "BTC" ? (bal * e.spot_quote_minor * 80n) / 100n / 10n ** 8n : bal,
    }));
    let vaultClaims = 0n;
    for (const [vid, vp] of acc.vault_positions) {
      const v = e.vaults.get(vid);
      if (!v || v.total_shares <= 0n) continue;
      const navPer = v.nav_quote_minor / v.total_shares;
      vaultClaims += vp.shares * navPer;
    }
    const cash = acc.cash_quote_minor > 0n ? acc.cash_quote_minor : 0n;
    const payload = `${d.id}|${cash}|${collateral.map((c) => `${c.code}:${c.balance_minor}`).join(",")}|${vaultClaims}|${nonce}`;
    payloads.push(payload);
    leaves.push(sha256Tagged("POC-LEAF", payload));
  }
  const { root } = buildMerkle(leaves);
  let total = 0n;
  rows.push(); // placeholder removed below
  rows.length = 0;
  payloads.forEach((payload, i) => {
    const d = ACCOUNT_DESCRIPTORS[i]!;
    const acc = e.accounts.get(d.id)!;
    const collateral = [...acc.collateral.entries()].map(([code, bal]) => ({
      code,
      balance_minor: bal,
      value_quote_minor: code === "BTC" ? (bal * e.spot_quote_minor * 80n) / 100n / 10n ** 8n : bal,
    }));
    let vaultClaims = 0n;
    for (const [vid, vp] of acc.vault_positions) {
      const v = e.vaults.get(vid);
      if (!v || v.total_shares <= 0n) continue;
      const navPer = v.nav_quote_minor / v.total_shares;
      vaultClaims += vp.shares * navPer;
    }
    const cash = acc.cash_quote_minor > 0n ? acc.cash_quote_minor : 0n;
    total += cash + vaultClaims + collateral.reduce((s, c) => s + c.value_quote_minor, 0n);
    rows.push({
      subaccount: d.id,
      quote_cash_minor: cash,
      collateral,
      vault_claims_quote_minor: vaultClaims,
      leaf_hash: leaves[i]!,
      inclusion_proof: proveInclusion(leaves, i).siblings.map((s) => ({ hash: s.hash, right: s.right })),
    });
  });
  const report: PorReport = {
    nonce,
    ts: e.now,
    root,
    total_liabilities_quote_minor: total,
    entry_count: rows.length,
    attested_reserve_quote_minor: e.insurance_balance + total * 3n, // house attestation stub
    coverage_permille: null,
  };
  return { report, rows };
}

/* ═════════════════════════ full snapshot ═════════════════════════ */

export function buildSnapshot(e: SimEngine): VenueSnapshot {
  const accounts = buildAccountViews(e);
  const positions: Record<SubaccountId, PositionRow[]> = {};
  const greeks: Record<SubaccountId, { symbol: Symbol; delta: number; gamma: number; vega: number; theta: number }[]> = {};
  const vaultPositions: Record<SubaccountId, { vault_id: number; subaccount: number; shares: bigint; value_quote_minor: bigint; last_claim_quote_minor: bigint }[]> = {};
  const marginSummaries: Record<SubaccountId, VenueSnapshot["marginSummaries"][SubaccountId]> = {};
  const healths: Record<SubaccountId, "healthy" | "restricted" | "liquidation"> = {};
  for (const d of ACCOUNT_DESCRIPTORS) {
    positions[d.id] = buildPositionRows(e, d.id);
    greeks[d.id] = e.greeksFor(d.id);
    const m = e.marginOf(d.id);
    marginSummaries[d.id] = {
      equity_quote_minor: m.equity,
      maintenance_quote_minor: m.maintenance,
      initial_quote_minor: m.initial,
      order_margin_quote_minor: m.order_margin,
    };
    healths[d.id] = m.health;
    const vp: { vault_id: number; subaccount: number; shares: bigint; value_quote_minor: bigint; last_claim_quote_minor: bigint }[] = [];
    for (const [vid, pos] of e.accounts.get(d.id)?.vault_positions ?? []) {
      const v = e.vaults.get(vid);
      if (!v || v.total_shares <= 0n) continue;
      const navPer = v.nav_quote_minor / v.total_shares;
      vp.push({ vault_id: vid, subaccount: d.id, shares: pos.shares, value_quote_minor: pos.shares * navPer, last_claim_quote_minor: pos.last_claim_quote_minor });
    }
    vaultPositions[d.id] = vp;
  }

  const openOrders: Record<SubaccountId, OpenOrderRow[]> = {};
  for (const d of ACCOUNT_DESCRIPTORS) {
    openOrders[d.id] = buildOpenOrderRows(e, d.id);
  }

  const mmStates = [...e.mmStates.entries()].map(([sub, st]) => ({
    subaccount: sub,
    enrolled: st.enrolled,
    tier: st.tier,
    fee_discount_bps: st.discount_bps,
    uptime_permille: st.uptime_permille,
    samples: st.samples.length,
    two_sided_samples: st.samples.filter((s) => s.ok).length,
    last_review_ts: st.next_review_at - 24 * 3600 * 1000,
  }));

  const rfqs = [...e.rfqs.values()].map((r) => ({
    rfq_id: r.rfq_id,
    taker: r.taker,
    legs: r.legs,
    counterparties: r.counterparties,
    min_total_cost_quote_minor: r.min_total_cost_quote_minor,
    max_total_cost_quote_minor: r.max_total_cost_quote_minor,
    created_at: r.created_at,
    expires_at: r.expires_at,
    status: r.status,
    quotes: r.quotes,
    executed_quote_id: r.executed_quote_id,
    trades: r.trades ?? null,
  }));

  return {
    meta: {
      now: e.now,
      speed: e.running ? e.speed : 0,
      running: e.running,
      seed: e.config.seed,
      phase: e.phase,
      started_at: e.startedAt,
      ticks: e.ticks,
      connected: true,
      transport: "sim-worker",
    },
    instruments: [...e.instruments.values()],
    markets: buildMarketRows(e),
    accounts,
    accountDescriptors: ACCOUNT_DESCRIPTORS,
    positions,
    greeks,
    collateral: buildCollateral(e),
    openOrders,
    fills: buildFills(e),
    stats: { ...e.stats, insurance_balance: e.insurance_balance },
    fundingHistory: e.fundingHistory.slice(-200),
    oracle: { BTC: e.oracle.snapshot(e.now) },
    vaults: [...e.vaults.values()].map((v) => ({
      vault_id: v.vault_id,
      revenue_share_bps: v.revenue_share_bps,
      total_shares: v.total_shares,
      nav_quote_minor: v.nav_quote_minor,
      nav_per_share_quote_minor: v.total_shares > 0n ? v.nav_quote_minor / v.total_shares : 1_00n,
      pending_subscriptions_quote_minor: v.pending_subs.reduce((s, x) => s + x.amount, 0n),
      pending_redemptions_shares: v.pending_redeems.reduce((s, x) => s + x.shares, 0n),
      epoch: v.epoch,
      last_epoch_ts: v.last_epoch_ts,
      lifetime_credit_quote_minor: v.lifetime_credit,
    })),
    vaultPositions,
    rfqs,
    blocks: e.blocks.map((b) => ({
      block_id: b.block_id,
      taker: b.taker,
      maker: b.maker,
      legs: b.legs,
      total_notional_quote_minor: b.total_notional,
      registered_ts: b.registered_ts,
      broadcast_ts: b.broadcast_ts,
      printed: b.printed,
    })),
    withdrawals: e.withdrawals.list(),
    proposals: e.governance.list(),
    marginSummaries,
    healths,
    mmStates,
    incentiveScoreboard: [],
    incentiveParams: {
      max_spread_bps: 50,
      min_size_lots: 1,
      two_sided_multiplier: 2,
      budget_quote_minor_per_hour: e.config.reward_budget_per_hour_quote_minor,
    },
    insurance: buildInsurance(e),
    breakers: [...e.breakers.entries()].map(([symbol, b]) => ({
      symbol,
      kind: b.kind,
      tripped_at: b.tripped_at,
      cooldown_until: b.until,
    })),
    mmp: {},
    twaps: [...e.twapParents.values()].map((p) => ({
      parent_id: p.parent_id,
      symbol: p.symbol,
      side: p.side,
      total_lots: p.total_lots,
      slices: p.slices,
      slices_placed: p.placed,
      lots_placed: p.lots_placed,
      next_slice_ts: p.next_slice_ts,
      state: p.state,
    })),
    por: e.porCached,
  };
}
