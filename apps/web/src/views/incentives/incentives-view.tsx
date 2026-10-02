"use client";

/**
 * Incentives — the economics engine page.
 *
 * Revenue router (60/30/10 with insurance overflow to buyback), the MM tier
 * program and its live measurement windows, the volume fee ladder, option
 * premium fee caps, the budgeted liquidity reward pool, and the rewards feed.
 */

import { memo, useMemo } from "react";
import { useVenueStore } from "@/lib/venue-store";
import { usd, relTime } from "@/lib/fmt";
import {
  FEE_TIERS,
  MM_TIERS,
  MM_MAX_DISCOUNT_BPS,
  OPTION_FEE_CAPS,
  REVENUE_SPLIT,
  formatPermille,
} from "@perp/types";
import { cn } from "@/lib/utils";
import { Trophy, Info, Shield, RotateCcw, Percent, Waves } from "lucide-react";

export const IncentivesView = memo(function IncentivesView() {
  const snapshot = useVenueStore((s) => s.snapshot);
  const journal = useVenueStore((s) => s.journal);

  const rev = snapshot?.stats.revenue;
  const house = rev?.house_quote_minor ?? 0n;
  const insurance = rev?.insurance_quote_minor ?? 0n;
  const buyback = rev?.buyback_quote_minor ?? 0n;
  const overflow = rev?.insurance_overflow_quote_minor ?? 0n;
  const total = house + insurance + buyback;
  const mmStates = snapshot?.mmStates ?? [];
  const descriptors = snapshot?.accountDescriptors ?? [];
  const params = snapshot?.incentiveParams;
  const now = snapshot?.meta.now ?? Date.now();

  /** Stacked-bar proportions — the ledger's exact split, 60/30/10 until fees flow. */
  const prop = (v: bigint, nominal: number): number => {
    if (total <= 0n) return nominal;
    return Number((v * 10_000n) / total) / 100;
  };
  const housePct = prop(house, 60);
  const insurancePct = prop(insurance, 30);
  const buybackPct = prop(buyback, 10);

  /** Reward payouts — newest first, capped at 40 rows. */
  const rewards = useMemo(() => {
    const rows: { sub: number; amount: bigint; ts: number }[] = [];
    for (let i = journal.length - 1; i >= 0 && rows.length < 40; i--) {
      const entry = journal[i];
      const ev = entry.event;
      if (ev.type !== "reward") continue;
      rows.push({ sub: ev.payload.subaccount, amount: ev.payload.amount_quote_minor, ts: entry.ts });
    }
    return rows;
  }, [journal]);

  return (
    <div className="h-full flex flex-col min-h-0">
      {/* Toolbar */}
      <div className="h-11 shrink-0 flex items-center gap-2.5 px-3 border-b border-hairline">
        <Trophy className="w-4 h-4 text-primary" />
        <h1 className="text-[13px] font-semibold">Incentives &amp; Fee Economics</h1>
        <div className="flex-1" />
        <p className="text-[10px] text-muted-foreground/60 font-mono hidden sm:block truncate">
          router 60/30/10 · {mmStates.filter((m) => m.enrolled).length} enrolled MMs
          {params ? ` · rewards ${usd(params.budget_quote_minor_per_hour)}/hr` : ""}
        </p>
      </div>

      <div className="flex-1 min-h-0 overflow-auto scroll-thin p-3">
        {/* ── Revenue router ── */}
        <section className="panel p-4" aria-label="Revenue router">
          <div className="flex items-center justify-between mb-3">
            <div className="flex items-center gap-2">
              <RotateCcw className="w-3.5 h-3.5 text-primary" />
              <h2 className="text-[10px] uppercase tracking-wider text-muted-foreground font-medium">
                Revenue router
              </h2>
            </div>
            <span className="text-[9px] font-mono text-muted-foreground/50">
              REVENUE_SPLIT {REVENUE_SPLIT.house_bps}/{REVENUE_SPLIT.insurance_bps}/
              {REVENUE_SPLIT.buyback_bps} bp · routed {usd(total, { compact: true })}
            </span>
          </div>

          <div className="grid grid-cols-1 sm:grid-cols-3 gap-3">
            <RouterTile
              label="House"
              share="60%"
              value={house}
              dotClass="bg-primary"
              valueClass="text-primary"
            />
            <RouterTile
              label="Insurance"
              share="30%"
              value={insurance}
              dotClass="bg-chart-3"
              valueClass="text-chart-3"
            />
            <RouterTile
              label="Buyback"
              share="10%"
              value={buyback}
              dotClass="bg-up"
              valueClass="text-up"
            />
          </div>

          {/* Stacked split bar */}
          <div
            className="mt-3 h-7 rounded-lg overflow-hidden flex border border-hairline"
            role="img"
            aria-label={`Revenue split bar: house ${housePct.toFixed(1)}%, insurance ${insurancePct.toFixed(1)}%, buyback ${buybackPct.toFixed(1)}%`}
          >
            <div
              className="bg-primary flex items-center px-2 overflow-hidden"
              style={{ width: `${housePct}%` }}
            >
              {housePct > 13 && (
                <span className="text-[9px] font-mono text-primary-foreground whitespace-nowrap">
                  house {housePct.toFixed(0)}%
                </span>
              )}
            </div>
            <div
              className="bg-chart-3 flex items-center px-2 overflow-hidden"
              style={{ width: `${insurancePct}%` }}
            >
              {insurancePct > 13 && (
                <span className="text-[9px] font-mono text-[#241a04] whitespace-nowrap">
                  insurance {insurancePct.toFixed(0)}%
                </span>
              )}
            </div>
            <div
              className="bg-up flex items-center px-2 overflow-hidden"
              style={{ width: `${buybackPct}%` }}
            >
              {buybackPct > 9 && (
                <span className="text-[9px] font-mono text-[#052018] whitespace-nowrap">
                  buyback {buybackPct.toFixed(0)}%
                </span>
              )}
            </div>
          </div>

          {overflow > 0n && (
            <p className="mt-2 flex items-center gap-1.5 text-[10.5px] text-chart-3">
              <Info className="w-3 h-3 shrink-0" />
              <span>
                coverage above 200% target — insurance share overflows to buyback: {usd(overflow)}
              </span>
            </p>
          )}
        </section>

        {/* ── MM tier program + live state ── */}
        <div className="grid gap-3 lg:grid-cols-2 mt-3 items-start">
          <section className="panel" aria-label="MM tier program">
            <header className="h-9 px-3 flex items-center justify-between border-b border-hairline">
              <h2 className="text-[10px] uppercase tracking-wider text-muted-foreground font-medium">
                MM tier program
              </h2>
              <span className="text-[9px] font-mono text-muted-foreground/50">
                discount bound {(MM_MAX_DISCOUNT_BPS / 100).toFixed(0)}%
              </span>
            </header>
            <div className="overflow-x-auto scroll-thin">
              <table className="w-full text-[11.5px] border-collapse">
                <thead>
                  <tr>
                    <Th>Tier</Th>
                    <Th right>Min uptime</Th>
                    <Th right>Max worst-side spread</Th>
                    <Th right>Min smaller side</Th>
                    <Th right>Fee discount</Th>
                  </tr>
                </thead>
                <tbody>
                  {MM_TIERS.map((t) => {
                    const live = mmStates.filter((m) => m.tier === t.tier).length;
                    return (
                      <tr
                        key={t.tier}
                        className={cn(
                          "hover:bg-muted/30 border-b border-hairline/40",
                          live > 0 && "bg-primary/[0.04]",
                        )}
                      >
                        <Td>
                          <span className="inline-flex items-center gap-1.5">
                            <span className="w-1 h-3.5 rounded-full bg-primary/70" />
                            <span className="font-mono text-[11px] font-medium">{t.tier}</span>
                            {live > 0 && (
                              <span className="text-[8px] px-1 rounded bg-primary/15 text-primary font-mono">
                                {live} live
                              </span>
                            )}
                          </span>
                        </Td>
                        <Td right>{formatPermille(t.min_uptime_permille)}</Td>
                        <Td right className="text-muted-foreground">
                          {t.max_worst_side_spread_bps.toLocaleString("en-US")} bp
                        </Td>
                        <Td right className="text-muted-foreground">
                          {t.min_smaller_side_lots} {t.min_smaller_side_lots === 1 ? "lot" : "lots"}
                        </Td>
                        <Td right className="text-primary">
                          {(t.fee_discount_bps / 100).toFixed(0)}%{" "}
                          <span className="text-muted-foreground text-[9px]">
                            ({t.fee_discount_bps.toLocaleString("en-US")}bp)
                          </span>
                        </Td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            </div>
          </section>

          <section className="panel" aria-label="MM live state">
            <header className="h-9 px-3 flex items-center justify-between border-b border-hairline">
              <h2 className="text-[10px] uppercase tracking-wider text-muted-foreground font-medium">
                MM live state
              </h2>
              <span className="text-[9px] font-mono text-muted-foreground/50">rolling window</span>
            </header>
            <div className="overflow-x-auto scroll-thin">
              <table className="w-full text-[11.5px] border-collapse">
                <thead>
                  <tr>
                    <Th>Subaccount</Th>
                    <Th>Enrolled</Th>
                    <Th>Tier</Th>
                    <Th right>Fee discount</Th>
                    <Th right>Uptime</Th>
                    <Th right>Samples</Th>
                    <Th right>Two-sided</Th>
                  </tr>
                </thead>
                <tbody>
                  {mmStates.length === 0 && (
                    <EmptyRow cols={7} text="No MMs enrolled — the tier program is idle." />
                  )}
                  {mmStates.map((m) => {
                    const d = descriptors.find((x) => x.id === m.subaccount);
                    const proMm = m.subaccount === 1 || m.subaccount === 2;
                    return (
                      <tr
                        key={m.subaccount}
                        className={cn(
                          "hover:bg-muted/30 border-b border-hairline/40",
                          proMm && "bg-primary/[0.04]",
                        )}
                      >
                        <Td>
                          <span className="flex items-center gap-1.5">
                            <span
                              className="w-1.5 h-1.5 rounded-full shrink-0"
                              style={{ background: d?.color ?? "#888" }}
                            />
                            <span className="font-medium">{d?.name ?? `Sub ${m.subaccount}`}</span>
                          </span>
                        </Td>
                        <Td>
                          {m.enrolled ? (
                            <span className="text-[8px] font-mono px-1.5 py-0.5 rounded border border-up/40 bg-up/10 text-up">
                              ENROLLED
                            </span>
                          ) : (
                            <span className="text-muted-foreground/50">—</span>
                          )}
                        </Td>
                        <Td className={m.tier ? "text-primary font-mono text-[11px]" : "text-muted-foreground/50"}>
                          {m.tier ?? "—"}
                        </Td>
                        <Td right className={m.fee_discount_bps > 0 ? "text-primary" : "text-muted-foreground/50"}>
                          {m.fee_discount_bps > 0
                            ? `${(m.fee_discount_bps / 100).toFixed(1)}%`
                            : "—"}
                        </Td>
                        <Td right>{formatPermille(m.uptime_permille)}</Td>
                        <Td right className="text-muted-foreground">
                          {m.samples.toLocaleString("en-US")}
                        </Td>
                        <Td right className="text-muted-foreground">
                          {m.two_sided_samples.toLocaleString("en-US")}
                        </Td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            </div>
          </section>
        </div>

        {/* ── Fee ladder · option caps · reward pool ── */}
        <div className="grid gap-3 lg:grid-cols-3 mt-3 items-start">
          <section className="panel" aria-label="Fee ladder">
            <header className="h-9 px-3 flex items-center justify-between border-b border-hairline">
              <h2 className="text-[10px] uppercase tracking-wider text-muted-foreground font-medium">
                Fee ladder
              </h2>
              <span className="text-[9px] font-mono text-muted-foreground/50">30d trailing volume</span>
            </header>
            <div className="overflow-x-auto scroll-thin">
              <table className="w-full text-[11.5px] border-collapse">
                <thead>
                  <tr>
                    <Th>Tier</Th>
                    <Th right>30d volume</Th>
                    <Th right>Maker</Th>
                    <Th right>Taker</Th>
                  </tr>
                </thead>
                <tbody>
                  {FEE_TIERS.map((t) => {
                    const rebate = t.maker_bps < 0;
                    return (
                      <tr
                        key={t.name}
                        className={cn(
                          "hover:bg-muted/30 border-b border-hairline/40",
                          rebate && "bg-up/[0.05]",
                        )}
                      >
                        <Td className="font-mono text-[11px]">{t.name}</Td>
                        <Td right className="text-muted-foreground">
                          {t.volume_threshold_quote_minor === 0n
                            ? "—"
                            : usd(t.volume_threshold_quote_minor, { compact: true })}
                        </Td>
                        <Td right>
                          {rebate ? (
                            <span className="text-up">
                              −{-t.maker_bps} bp{" "}
                              <span className="text-[8px] font-mono px-1 py-px rounded bg-up/15 text-up align-middle">
                                maker rebate
                              </span>
                            </span>
                          ) : (
                            `${t.maker_bps} bp`
                          )}
                        </Td>
                        <Td right>{t.taker_bps} bp</Td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            </div>
          </section>

          <section className="panel p-4 flex flex-col gap-3" aria-label="Option fee caps">
            <div className="flex items-center gap-2">
              <Percent className="w-3.5 h-3.5 text-primary" />
              <h2 className="text-[10px] uppercase tracking-wider text-muted-foreground font-medium">
                Option fee caps
              </h2>
            </div>
            <div className="space-y-2">
              <CapRow
                label="Takers"
                value={`≤ ${(OPTION_FEE_CAPS.taker_cap_bps / 100).toFixed(1)}% of premium`}
                hint="OPTION_FEE_CAPS.taker_cap_bps = 1250"
              />
              <CapRow
                label="Makers"
                value={`≤ ${(OPTION_FEE_CAPS.maker_cap_bps / 100).toFixed(1)}% of premium`}
                hint="OPTION_FEE_CAPS.maker_cap_bps = 250"
              />
            </div>
            <p className="num text-[10px] text-muted-foreground/70 border-t border-hairline pt-2.5">
              fee = min(rate × notional, cap × premium)
            </p>
            <p className="text-[10.5px] leading-relaxed text-muted-foreground">
              Option fills charge the lesser of the laddered rate on notional and the premium cap —
              a $1 premium never pays a $5 fee.
            </p>
          </section>

          <section className="panel p-4 flex flex-col gap-3" aria-label="Reward pool">
            <div className="flex items-center gap-2">
              <Waves className="w-3.5 h-3.5 text-primary" />
              <h2 className="text-[10px] uppercase tracking-wider text-muted-foreground font-medium">
                Reward pool
              </h2>
            </div>
            <div>
              <p className="text-[9px] uppercase tracking-wider text-muted-foreground/70">Budget / hour</p>
              <p className="num text-lg font-semibold mt-0.5">
                {params ? usd(params.budget_quote_minor_per_hour) : "—"}
              </p>
            </div>
            <div className="flex flex-wrap gap-1.5">
              <span className="text-[9px] font-mono px-1.5 py-0.5 rounded border border-hairline bg-muted/40 text-muted-foreground">
                spread ≤ {params?.max_spread_bps ?? 50} bp
              </span>
              <span className="text-[9px] font-mono px-1.5 py-0.5 rounded border border-hairline bg-muted/40 text-muted-foreground">
                size ≥ {params?.min_size_lots ?? 1} lot
              </span>
              <span className="text-[9px] font-mono px-1.5 py-0.5 rounded border border-hairline bg-muted/40 text-muted-foreground">
                two-sided ×{params?.two_sided_multiplier ?? 2}
              </span>
            </div>
            <p className="text-[10.5px] leading-relaxed text-muted-foreground">
              Scored on two-sided, tight quoting (≤{params?.max_spread_bps ?? 50}bp spread,{" "}
              ≥{params?.min_size_lots ?? 1} lot, {params?.two_sided_multiplier ?? 2}× for
              two-sided).
            </p>
          </section>
        </div>

        {/* ── Rewards feed ── */}
        <section className="panel mt-3" aria-label="Rewards feed">
          <header className="h-9 px-3 flex items-center justify-between border-b border-hairline">
            <h2 className="text-[10px] uppercase tracking-wider text-muted-foreground font-medium">
              Rewards feed
            </h2>
            <span className="text-[9px] font-mono text-muted-foreground/50">
              Event::RewardPaid · newest first · cap 40
            </span>
          </header>
          <div className="max-h-64 overflow-auto scroll-thin">
            <table className="w-full text-[11.5px] border-collapse">
              <thead>
                <tr>
                  <Th>Account</Th>
                  <Th right>Reward</Th>
                  <Th right>Paid</Th>
                </tr>
              </thead>
              <tbody>
                {rewards.length === 0 && (
                  <EmptyRow cols={3} text="No rewards paid yet — the pool accrues until the next settle." />
                )}
                {rewards.map((r, i) => {
                  const d = descriptors.find((x) => x.id === r.sub);
                  return (
                    <tr key={i} className="hover:bg-muted/30 border-b border-hairline/40">
                      <Td>
                        <span className="flex items-center gap-1.5">
                          <span
                            className="w-1.5 h-1.5 rounded-full shrink-0"
                            style={{ background: d?.color ?? "#888" }}
                          />
                          <span className="font-medium">{d?.name ?? `Sub ${r.sub}`}</span>
                        </span>
                      </Td>
                      <Td right className="text-up font-semibold">
                        {usd(r.amount, { sign: true })}
                      </Td>
                      <Td right className="text-muted-foreground/60 text-[10px] font-mono">
                        {relTime(r.ts, now)}
                      </Td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        </section>

        {/* ── Explainer ── */}
        <section className="panel mt-3 p-4" aria-label="How incentives work">
          <div className="flex items-center gap-2 mb-1.5">
            <Info className="w-3.5 h-3.5 text-primary" />
            <h2 className="text-[10px] uppercase tracking-wider text-muted-foreground font-medium">
              How incentives work
            </h2>
          </div>
          <p className="text-[11.5px] font-medium mb-1">Fees pay for liquidity, not the other way round.</p>
          <p className="text-[11px] leading-relaxed text-muted-foreground">
            MM tier discounts compose after the volume ladder, bounded at{" "}
            {(MM_MAX_DISCOUNT_BPS / 100).toFixed(0)}%, apply to every venue fee path (CLOB, auction,
            RFQ, blocks), demote on silence.
          </p>
        </section>
      </div>
    </div>
  );
});

/* ═════════════════════════ locals ═════════════════════════ */

function RouterTile({
  label,
  share,
  value,
  dotClass,
  valueClass,
}: {
  label: string;
  share: string;
  value: bigint;
  dotClass: string;
  valueClass: string;
}) {
  return (
    <div className="panel-2 p-3">
      <div className="flex items-center justify-between">
        <span className="flex items-center gap-1.5 text-[9px] uppercase tracking-wider text-muted-foreground">
          <span className={cn("w-2 h-2 rounded-full", dotClass)} />
          {label}
        </span>
        <span className={cn("num text-[10px] font-mono", valueClass)}>{share}</span>
      </div>
      <p className={cn("num text-lg font-semibold mt-1", valueClass)}>{usd(value)}</p>
    </div>
  );
}

function CapRow({ label, value, hint }: { label: string; value: string; hint: string }) {
  return (
    <div className="flex items-center justify-between gap-2 rounded-md border border-hairline bg-muted/25 px-2.5 py-2">
      <span className="flex items-center gap-1.5 text-[11px] text-muted-foreground">
        <Shield className="w-3 h-3 text-muted-foreground/70" />
        {label}
      </span>
      <span className="num text-[11.5px] font-semibold" title={hint}>
        {value}
      </span>
    </div>
  );
}

function Th({ children, right }: { children: React.ReactNode; right?: boolean }) {
  return (
    <th
      className={cn(
        "sticky top-0 z-10 bg-panel backdrop-blur-sm text-[9px] uppercase tracking-wider text-muted-foreground/70 font-medium py-1.5 px-2.5 border-b border-hairline whitespace-nowrap",
        right ? "text-right" : "text-left",
      )}
    >
      {children}
    </th>
  );
}

function Td({ children, right, className }: { children: React.ReactNode; right?: boolean; className?: string }) {
  return (
    <td className={cn("py-1.5 px-2.5 whitespace-nowrap", right ? "text-right num" : "", className ?? "")}>
      {children}
    </td>
  );
}

function EmptyRow({ cols, text }: { cols: number; text: string }) {
  return (
    <tr>
      <td colSpan={cols} className="py-10 text-center text-muted-foreground/50 text-[11px]">
        {text}
      </td>
    </tr>
  );
}
