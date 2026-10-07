"use client";

/**
 * Risk — the safety net.
 * Insurance fund balance + coverage gauge, the fund's inventory, an
 * all-accounts health board sorted worst-first, circuit breakers, the
 * live liquidation/ADL feed from the journal, and the cascade order.
 */

import { memo, useMemo } from "react";
import { useVenueStore } from "@/lib/venue-store";
import { usd, usdCompact, shortSymbol, healthClass, healthLabel, simTime, relTime } from "@/lib/fmt";
import type { AccountDescriptor } from "@perp/types";
import { Boxes, Flame, HeartPulse, Landmark, ShieldCheck, Zap } from "lucide-react";
import { cn } from "@/lib/utils";

const COVERAGE_TARGET_PERMILLE = 2000;
const COVERAGE_BOOST_THRESHOLD = 500;

export const RiskView = memo(function RiskView() {
  const snapshot = useVenueStore((s) => s.snapshot);
  const journal = useVenueStore((s) => s.journal);
  const account = useVenueStore((s) => s.activeAccount);

  const now = snapshot?.meta.now ?? Date.now();
  const insurance = snapshot?.insurance ?? null;
  const breakers = snapshot?.breakers ?? [];
  const descriptors = snapshot?.accountDescriptors ?? [];
  /** Liquidation + ADL events from the journal, newest first. */
  const liqRows = useMemo(
    () =>
      journal
        .filter((j) => j.event.type === "liquidation" || j.event.type === "adl")
        .slice(-80)
        .reverse(),
    [journal],
  );

  /** All accounts, worst equity/maintenance ratio first. */
  const healthRows = useMemo(() => {
    const ds = snapshot?.accountDescriptors ?? [];
    const rows = (snapshot?.accounts ?? []).map((a) => {
      const d = ds.find((x) => x.id === a.id) ?? null;
      const ratio =
        a.summary.maintenance_quote_minor > 0n
          ? Number(a.summary.equity_quote_minor) / Number(a.summary.maintenance_quote_minor)
          : Number.POSITIVE_INFINITY;
      return { a, d, ratio };
    });
    return rows.sort((x, y) => x.ratio - y.ratio);
  }, [snapshot]);

  const coverage = insurance?.coverage_permille ?? 0;
  const coverageClass =
    coverage < COVERAGE_BOOST_THRESHOLD
      ? "text-down"
      : coverage < COVERAGE_TARGET_PERMILLE
        ? "text-amber-400"
        : "text-up";
  const coverageBarClass =
    coverage < COVERAGE_BOOST_THRESHOLD ? "bg-down" : coverage < COVERAGE_TARGET_PERMILLE ? "bg-amber-400" : "bg-up";

  return (
    <div className="h-full flex flex-col min-h-0">
      {/* Toolbar */}
      <div className="h-11 shrink-0 flex items-center gap-2.5 px-3 border-b border-hairline">
        <span className="text-[10px] text-muted-foreground">SFPM · MM = IM × 0.75</span>
        <span className="text-[9.5px] text-muted-foreground/60 hidden sm:inline">
          healthy ≥ IM · restricted ≥ MM · liquidation &lt; MM
        </span>
        <div className="flex-1" />
        <p className="text-[10px] text-muted-foreground/60 font-mono">
          {healthRows.length} accounts · {liqRows.length} liq/adl events in journal window
        </p>
      </div>

      <div className="flex-1 min-h-0 overflow-auto scroll-thin p-3">
        {/* Insurance fund */}
        <section aria-label="Insurance fund" className="panel mb-3 overflow-hidden">
          <header className="px-3 py-2 border-b border-hairline flex items-center gap-2">
            <Landmark className="w-3.5 h-3.5 text-chart-3" aria-hidden />
            <h2 className="text-[11px] font-semibold tracking-tight">Insurance fund</h2>
            <span className="text-[9.5px] text-muted-foreground/70 hidden sm:inline">
              buyer of last resort · capitalized from the fee router (30% of revenue)
            </span>
          </header>
          {insurance ? (
            <div className="p-3">
              <div className="grid grid-cols-2 sm:grid-cols-3 xl:grid-cols-5 gap-2 mb-3">
                <StatTile label="Balance" value={usd(insurance.balance_quote_minor)} strong />
                <StatTile
                  label="Coverage"
                  value={`${(coverage / 10).toFixed(1)}%`}
                  valueClass={coverageClass}
                  sub={`${coverage}‰ of liabilities`}
                />
                <StatTile label="Liquidations" value={String(insurance.liquidations)} sub="lifetime" />
                <StatTile label="ADLs" value={String(insurance.adls)} sub="lifetime" />
                <StatTile label="Total absorbed" value={usdCompact(insurance.total_absorbed_quote_minor)} />
              </div>
              <div>
                <div className="flex items-center justify-between mb-1">
                  <p className="text-[9px] uppercase tracking-wider text-muted-foreground">
                    Coverage vs 2000‰ target
                  </p>
                  <p className="text-[9.5px] font-mono text-muted-foreground num">
                    {coverage}‰ / {COVERAGE_TARGET_PERMILLE}‰
                  </p>
                </div>
                <div
                  className="relative h-2.5 rounded-full bg-muted overflow-hidden"
                  role="progressbar"
                  aria-label="Insurance coverage against the 2000 permille target"
                  aria-valuemin={0}
                  aria-valuemax={COVERAGE_TARGET_PERMILLE}
                  aria-valuenow={Math.min(coverage, COVERAGE_TARGET_PERMILLE)}
                >
                  <div
                    className={cn(
                      "absolute inset-y-0 left-0 rounded-full transition-all duration-500",
                      coverageBarClass,
                    )}
                    style={{ width: `${Math.min(100, (coverage / COVERAGE_TARGET_PERMILLE) * 100)}%` }}
                  />
                  <div
                    className="absolute inset-y-0 w-px bg-muted-foreground/50"
                    style={{ left: `${(COVERAGE_BOOST_THRESHOLD / COVERAGE_TARGET_PERMILLE) * 100}%` }}
                    title="penalty boost threshold — 500‰"
                  />
                </div>
                <p className="text-[9.5px] text-muted-foreground/70 mt-1.5 flex items-center gap-1.5">
                  <Zap className="w-3 h-3 text-chart-3 shrink-0" aria-hidden />
                  liquidation penalty 1.25% × coverage ladder boost (1.5× below 500‰)
                </p>
              </div>
            </div>
          ) : (
            <EmptyBlock text="Insurance state not streamed yet — awaiting the first snapshot." />
          )}
        </section>

        {/* Inventory + account health */}
        <div className="grid grid-cols-1 xl:grid-cols-12 gap-3 mb-3">
          <section aria-label="Insurance inventory" className="panel xl:col-span-4 overflow-hidden flex flex-col">
            <header className="px-3 py-2 border-b border-hairline flex items-center gap-2">
              <Boxes className="w-3.5 h-3.5 text-muted-foreground" aria-hidden />
              <h2 className="text-[11px] font-semibold tracking-tight">Insurance inventory</h2>
              <div className="flex-1" />
              <span className="text-[9px] font-mono text-muted-foreground/60">
                {insurance?.inventory.length ?? 0} positions
              </span>
            </header>
            <div className="flex-1 min-h-0 overflow-auto scroll-thin max-h-72">
              {(insurance?.inventory.length ?? 0) === 0 ? (
                <div className="h-full min-h-32 flex items-center justify-center text-muted-foreground/50 text-[11px] px-6 text-center">
                  No inventory — the fund only takes positions when the book can't absorb a liquidation.
                </div>
              ) : (
                <table className="w-full text-[11.5px] border-collapse">
                  <thead>
                    <tr>
                      <Th>Instrument</Th>
                      <Th right>Lots</Th>
                      <Th right>Mark</Th>
                    </tr>
                  </thead>
                  <tbody>
                    {insurance?.inventory.map((row) => (
                      <tr key={row.symbol} className="hover:bg-muted/30 border-b border-hairline/40">
                        <Td className="font-mono text-[11px]">{shortSymbol(row.symbol)}</Td>
                        <Td
                          right
                          className={row.signed_lots > 0 ? "text-up" : row.signed_lots < 0 ? "text-down" : ""}
                        >
                          {row.signed_lots > 0 ? "+" : ""}
                          {row.signed_lots}
                        </Td>
                        <Td right className="text-muted-foreground">{usd(row.mark_quote_minor)}</Td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              )}
            </div>
          </section>

          <section
            aria-label="Account health board"
            className="panel xl:col-span-8 overflow-hidden flex flex-col"
          >
            <header className="px-3 py-2 border-b border-hairline flex items-center gap-2">
              <HeartPulse className="w-3.5 h-3.5 text-muted-foreground" aria-hidden />
              <h2 className="text-[11px] font-semibold tracking-tight">Account health</h2>
              <span className="text-[9.5px] text-muted-foreground/70 hidden sm:inline">
                equity ÷ maintenance · worst first
              </span>
              <div className="flex-1" />
              <span className="text-[9px] font-mono text-muted-foreground/60">
                green &gt; 1.3 · amber 1.0–1.3 · red &lt; 1.0
              </span>
            </header>
            <div className="flex-1 min-h-0 overflow-auto scroll-thin max-h-96">
              <table className="w-full text-[11.5px] border-collapse">
                <thead>
                  <tr>
                    <Th>Account</Th>
                    <Th right>Equity</Th>
                    <Th right>Maintenance</Th>
                    <Th right>Ratio</Th>
                    <Th>Health</Th>
                  </tr>
                </thead>
                <tbody>
                  {healthRows.map(({ a, d, ratio }) => {
                    const barClass =
                      ratio > 1.3 ? "bg-up" : ratio >= 1.0 ? "bg-amber-400" : "bg-down";
                    return (
                      <tr
                        key={a.id}
                        className={cn(
                          "border-b border-hairline/40 hover:bg-muted/25",
                          a.id === account && "bg-primary/[0.07]",
                        )}
                      >
                        <Td>
                          <span className="flex items-center gap-1.5">
                            <span
                              className="w-1.5 h-1.5 rounded-full shrink-0"
                              style={{ background: d?.color ?? "#888" }}
                            />
                            <span className="text-[11px]">{d?.name ?? `Sub ${a.id}`}</span>
                          </span>
                        </Td>
                        <Td right>{usd(a.summary.equity_quote_minor)}</Td>
                        <Td right className="text-muted-foreground">
                          {usd(a.summary.maintenance_quote_minor)}
                        </Td>
                        <Td right>
                          <span className="flex items-center justify-end gap-2">
                            <span
                              className="relative h-1.5 w-16 rounded-full bg-muted overflow-hidden"
                              aria-hidden
                            >
                              <span
                                className={cn("absolute inset-y-0 left-0 rounded-full", barClass)}
                                style={{
                                  width: `${ratio === Number.POSITIVE_INFINITY ? 100 : Math.min(100, (ratio / 2) * 100)}%`,
                                }}
                              />
                            </span>
                            <span className="num text-[11px]">
                              {ratio === Number.POSITIVE_INFINITY ? "∞" : `${ratio.toFixed(2)}×`}
                            </span>
                          </span>
                        </Td>
                        <Td className={healthClass(a.health)}>{healthLabel(a.health)}</Td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            </div>
          </section>
        </div>

        {/* Liquidation feed + circuit breakers */}
        <div className="grid grid-cols-1 xl:grid-cols-12 gap-3 mb-3">
          <section aria-label="Liquidation and ADL feed" className="panel xl:col-span-8 overflow-hidden flex flex-col">
            <header className="px-3 py-2 border-b border-hairline flex items-center gap-2">
              <Flame className="w-3.5 h-3.5 text-chart-3" aria-hidden />
              <h2 className="text-[11px] font-semibold tracking-tight">Liquidation & ADL feed</h2>
              <span className="text-[9.5px] text-muted-foreground/70 hidden sm:inline">
                journal · Event::LiquidationExecuted / Event::AdlExecuted
              </span>
              <div className="flex-1" />
              <span className="text-[9px] font-mono text-muted-foreground/60">
                {liqRows.length} events · newest first
              </span>
            </header>
            <div className="flex-1 min-h-0 overflow-auto scroll-thin max-h-96">
              {liqRows.length === 0 ? (
                <div className="h-full min-h-32 flex flex-col items-center justify-center gap-2 text-muted-foreground/50 px-6 text-center">
                  <Flame className="w-5 h-5" aria-hidden />
                  <p className="text-[11px]">
                    No liquidations yet — the scripted +6min shock (Degen-6 at 20× leverage) lights this feed up.
                  </p>
                </div>
              ) : (
                <table className="w-full text-[11.5px] border-collapse">
                  <thead>
                    <tr>
                      <Th>Type</Th>
                      <Th>Account</Th>
                      <Th>Instrument</Th>
                      <Th right>Lots</Th>
                      <Th right>Price</Th>
                      <Th>→ Insurance</Th>
                      <Th right>Penalty</Th>
                      <Th right>Time</Th>
                    </tr>
                  </thead>
                  <tbody>
                    {liqRows.map((j) => {
                      const ev = j.event;
                      if (ev.type === "liquidation") {
                        const p = ev.payload;
                        return (
                          <tr key={j.seq} className="hover:bg-muted/30 border-b border-hairline/40">
                            <Td>
                              <TypeBadge adl={false} />
                            </Td>
                            <Td>
                              <AccountCell id={p.subaccount} descriptors={descriptors} />
                            </Td>
                            <Td className="font-mono text-[11px]">{shortSymbol(p.symbol)}</Td>
                            <Td right>{p.lots}</Td>
                            <Td right>{usd(p.price_quote_minor)}</Td>
                            <Td>
                              {p.to_insurance ? (
                                <span className="text-[8px] font-mono px-1 py-px rounded border border-chart-3/30 bg-chart-3/10 text-chart-3">
                                  INS
                                </span>
                              ) : (
                                <span className="text-muted-foreground/40">—</span>
                              )}
                            </Td>
                            <Td right className="text-muted-foreground">
                              {usd(p.penalty_quote_minor)}
                            </Td>
                            <Td right className="text-muted-foreground/60 text-[10px] font-mono">
                              {simTime(j.ts)}
                            </Td>
                          </tr>
                        );
                      }
                      if (ev.type !== "adl") return null;
                      const p = ev.payload;
                      return (
                        <tr key={j.seq} className="hover:bg-muted/30 border-b border-hairline/40">
                          <Td>
                            <TypeBadge adl />
                          </Td>
                          <Td>
                            <AccountCell id={p.liquidated_subaccount} descriptors={descriptors} />
                          </Td>
                          <Td className="font-mono text-[11px]">{shortSymbol(p.symbol)}</Td>
                          <Td right>{p.lots}</Td>
                          <Td right title="bankruptcy price">
                            {usd(p.price_quote_minor)}
                          </Td>
                          <Td>
                            <span className="text-muted-foreground/40">—</span>
                          </Td>
                          <Td right className="text-muted-foreground/40">
                            —
                          </Td>
                          <Td right className="text-muted-foreground/60 text-[10px] font-mono">
                            {simTime(j.ts)}
                          </Td>
                        </tr>
                      );
                    })}
                  </tbody>
                </table>
              )}
            </div>
          </section>

          <section aria-label="Circuit breakers" className="panel xl:col-span-4 overflow-hidden flex flex-col">
            <header className="px-3 py-2 border-b border-hairline flex items-center gap-2">
              <Zap className="w-3.5 h-3.5 text-chart-3" aria-hidden />
              <h2 className="text-[11px] font-semibold tracking-tight">Circuit breakers</h2>
              <div className="flex-1" />
              <span className="text-[9px] font-mono text-muted-foreground/60">{breakers.length} active</span>
            </header>
            <div className="flex-1 min-h-0 overflow-auto scroll-thin">
              {breakers.length === 0 ? (
                <div className="h-full min-h-40 flex flex-col items-center justify-center gap-2 text-muted-foreground/50 px-6 text-center">
                  <ShieldCheck className="w-5 h-5" aria-hidden />
                  <p className="text-[11px]">
                    No breakers tripped — BBO/oracle dislocation monitor armed
                  </p>
                </div>
              ) : (
                breakers.map((b) => (
                  <div key={b.symbol} className="px-3 py-2 border-b border-hairline/40 hover:bg-muted/25">
                    <div className="flex items-center gap-2">
                      <span className="font-mono text-[11px]">{shortSymbol(b.symbol)}</span>
                      <span className="text-[8px] font-mono px-1 py-px rounded border border-chart-3/30 bg-chart-3/10 text-chart-3">
                        {b.kind === "price-dislocation" ? "BBO DISLOCATION" : "CASCADE VELOCITY"}
                      </span>
                    </div>
                    <p className="text-[9.5px] text-muted-foreground mt-0.5 num">
                      tripped {relTime(b.tripped_at, now)} · cooldown ends {relTime(b.cooldown_until, now)}
                    </p>
                  </div>
                ))
              )}
            </div>
          </section>
        </div>

        {/* Cascade explainer */}
        <section aria-label="Liquidation cascade order" className="panel p-3">
          <h2 className="text-[11px] font-semibold tracking-tight mb-2.5 flex items-center gap-1.5">
            <Landmark className="w-3.5 h-3.5 text-chart-3" aria-hidden />
            The liquidation cascade — in order
          </h2>
          <ol className="grid grid-cols-1 md:grid-cols-2 xl:grid-cols-4 gap-2.5 list-none p-0 m-0">
            {[
              {
                n: "01",
                title: "Partial first",
                body: "Reduce-only market orders work the position down against resting liquidity.",
              },
              {
                n: "02",
                title: "Book liquidity",
                body: "Book liquidity at prices better than the penalized mark is consumed before anything else.",
              },
              {
                n: "03",
                title: "Insurance buys",
                body: "Insurance acts as buyer of last resort at the penalized price — inventory lands on the fund.",
              },
              {
                n: "04",
                title: "ADL closes it out",
                body: "ADL against the most profitable counterparties at the bankruptcy price, only if the fund is spent.",
              },
            ].map((step) => (
              <li key={step.n} className="panel-2 p-2.5">
                <p className="text-[9px] font-mono text-primary mb-1">{step.n}</p>
                <p className="text-[11px] font-medium mb-0.5">{step.title}</p>
                <p className="text-[10px] text-muted-foreground leading-relaxed">{step.body}</p>
              </li>
            ))}
          </ol>
        </section>
      </div>
    </div>
  );
});

/* ─────────────────────────── local bits ─────────────────────────── */

function StatTile({
  label,
  value,
  valueClass,
  sub,
  strong,
}: {
  label: string;
  value: string;
  valueClass?: string;
  sub?: string;
  strong?: boolean;
}) {
  return (
    <div className="panel-2 p-3 flex flex-col justify-center">
      <p className="text-[9px] uppercase tracking-wider text-muted-foreground">{label}</p>
      <p className={cn("num", strong ? "text-lg font-semibold" : "text-base font-medium", valueClass)}>{value}</p>
      {sub && <p className="text-[9.5px] text-muted-foreground/70 mt-0.5">{sub}</p>}
    </div>
  );
}

function TypeBadge({ adl }: { adl: boolean }) {
  return adl ? (
    <span className="text-[8px] font-mono px-1.5 py-px rounded border border-amber-400/30 bg-amber-400/10 text-amber-400">
      ADL
    </span>
  ) : (
    <span className="text-[8px] font-mono px-1.5 py-px rounded border border-down/30 bg-down/10 text-down">LIQ</span>
  );
}

function AccountCell({ id, descriptors }: { id: number; descriptors: AccountDescriptor[] }) {
  const d = descriptors.find((x) => x.id === id);
  return (
    <span className="flex items-center gap-1.5">
      <span className="w-1.5 h-1.5 rounded-full shrink-0" style={{ background: d?.color ?? "#888" }} />
      <span className="text-[11px]">{d?.name ?? `Sub ${id}`}</span>
    </span>
  );
}

function EmptyBlock({ text }: { text: string }) {
  return <div className="py-12 text-center text-muted-foreground/50 text-[11px]">{text}</div>;
}

function Th({ children, right }: { children: React.ReactNode; right?: boolean }) {
  return (
    <th
      className={`sticky top-0 z-10 bg-panel backdrop-blur-sm text-[9px] uppercase tracking-wider text-muted-foreground/70 font-medium py-1.5 px-2.5 border-b border-hairline whitespace-nowrap ${
        right ? "text-right" : "text-left"
      }`}
    >
      {children}
    </th>
  );
}

function Td({ children, right, className, title }: { children: React.ReactNode; right?: boolean; className?: string; title?: string }) {
  return (
    <td
      title={title}
      className={`py-1.5 px-2.5 whitespace-nowrap ${right ? "text-right num" : ""} ${className ?? ""}`}
    >
      {children}
    </td>
  );
}
