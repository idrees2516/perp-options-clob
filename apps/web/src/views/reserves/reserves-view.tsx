"use client";

/**
 * Proof of reserves (G-35) — nonce-bound merkle liability trees over
 * cash + haircut collateral + vault claims. The verification runs here,
 * in the browser: we rebuild each leaf from the row's own data and fold
 * the inclusion proof up to the published root with the same
 * domain-tagged SHA-256 the settlement crate uses.
 */

import { memo, useMemo, useState } from "react";
import { useVenueStore } from "@/lib/venue-store";
import { getVenueClient } from "@perp/api-client";
import { usd, usdCompact, simDate, simTime } from "@/lib/fmt";
import { formatMoney } from "@perp/types";
import type { PorLiabilityRow, PorReport } from "@perp/types";
import { sha256Tagged, verifyInclusion } from "@perp/sim-engine";
import { toast } from "sonner";
import { Button } from "@/components/ui/button";
import { Check, Copy, FileLock2, ShieldCheck } from "lucide-react";

export const ReservesView = memo(function ReservesView() {
  const snapshot = useVenueStore((s) => s.snapshot);
  const report = snapshot?.por.report ?? null;
  const rows = snapshot?.por.rows ?? [];
  const descriptors = useMemo(() => {
    const m = new Map<number, string>();
    for (const d of snapshot?.accountDescriptors ?? []) m.set(d.id, d.name);
    return m;
  }, [snapshot]);

  return (
    <div className="h-full flex flex-col min-h-0">
      {/* toolbar */}
      <div className="h-11 shrink-0 flex items-center gap-2.5 px-3 border-b border-hairline">
        <FileLock2 className="w-3.5 h-3.5 text-primary" />
        <h2 className="text-[12px] font-semibold tracking-tight">Proof of reserves</h2>
        <span className="text-[9px] uppercase tracking-wider text-muted-foreground/60 font-mono">
          G-35 · merkleized liabilities
        </span>
        <div className="flex-1" />
        {report && (
          <p className="text-[10px] text-muted-foreground/60 font-mono">
            report #{report.nonce} · {simDate(report.ts)} {simTime(report.ts)}
          </p>
        )}
      </div>

      {/* content */}
      <div className="flex-1 min-h-0 overflow-auto scroll-thin p-3">
        {!report ? (
          <BuildReportPanel />
        ) : (
          <div className="flex flex-col gap-3 max-w-[1440px]">
            <ReportTiles report={report} />
            <div className="grid grid-cols-1 xl:grid-cols-[minmax(0,1fr)_330px] gap-3 items-start">
              <LiabilityTable report={report} rows={rows} descriptorOf={(id) => descriptors.get(id) ?? `sub ${id}`} />
              <MerkleTreePanel report={report} rows={rows} />
            </div>
            <ExplainerCard />
          </div>
        )}
      </div>
    </div>
  );
});

/* ─────────────────────────── build CTA ─────────────────────────── */

function BuildReportPanel() {
  return (
    <div className="flex flex-col gap-3 max-w-[1440px]">
      <section className="panel panel-glow p-10 flex flex-col items-center justify-center gap-4 min-h-[300px]" aria-label="Build report">
        <FileLock2 className="w-6 h-6 text-primary/70" />
        <div className="text-center space-y-1.5">
          <p className="text-sm font-medium">No report published yet</p>
          <p className="text-[11px] text-muted-foreground">
            Build a nonce-bound liability tree over every subaccount — the report lands on the next snapshot.
          </p>
        </div>
        <Button
          size="sm"
          className="h-9 text-[12px] font-semibold"
          onClick={() => {
            getVenueClient().send({ type: "por_build" });
            toast("Building nonce-bound liability tree…", {
              description: "POC-LEAF domain tags · sha256 · report arrives on the next snapshot",
            });
          }}
        >
          <ShieldCheck className="w-3.5 h-3.5" />
          Build report
        </Button>
        <p className="text-[9.5px] font-mono text-muted-foreground/50">
          client.send({"{"} type: "por_build" {"}"})
        </p>
      </section>
      <ExplainerCard />
    </div>
  );
}

/* ─────────────────────────── report tiles ─────────────────────────── */

function ReportTiles({ report }: { report: PorReport }) {
  const coverage =
    report.coverage_permille != null
      ? `${(report.coverage_permille / 10).toFixed(1)}%`
      : report.total_liabilities_quote_minor > 0n
        ? `${((Number(report.attested_reserve_quote_minor ?? 0n) / Number(report.total_liabilities_quote_minor)) * 100).toFixed(1)}%`
        : "—";
  return (
    <div className="grid grid-cols-2 sm:grid-cols-3 lg:grid-cols-6 gap-2.5">
      <div className="panel p-3 col-span-2 sm:col-span-3 lg:col-span-2">
        <p className="text-[9px] uppercase tracking-wider text-muted-foreground mb-1">Merkle root</p>
        <p className="num text-[12px] font-semibold truncate" title={report.root}>
          {report.root.slice(0, 22)}…
        </p>
      </div>
      <Tile label="Nonce" value={String(report.nonce)} />
      <Tile label="Entries" value={String(report.entry_count)} />
      <Tile label="Liabilities" value={usdCompact(report.total_liabilities_quote_minor)} />
      <Tile label="Attested reserves" value={usdCompact(report.attested_reserve_quote_minor)} />
      <Tile label="Coverage" value={coverage} className="text-up" />
    </div>
  );
}

function Tile({ label, value, className }: { label: string; value: string; className?: string }) {
  return (
    <div className="panel p-3">
      <p className="text-[9px] uppercase tracking-wider text-muted-foreground mb-1">{label}</p>
      <p className={`num text-lg font-semibold ${className ?? ""}`}>{value}</p>
    </div>
  );
}

/* ─────────────────────────── liability table ─────────────────────────── */

/** Rebuild the exact POC-LEAF payload the settlement crate hashed. */
function leafPayload(row: PorLiabilityRow, nonce: number): string {
  return `${row.subaccount}|${row.quote_cash_minor}|${row.collateral
    .map((c) => `${c.code}:${c.balance_minor}`)
    .join(",")}|${row.vault_claims_quote_minor}|${nonce}`;
}

function LiabilityTable({
  report,
  rows,
  descriptorOf,
}: {
  report: PorReport;
  rows: PorLiabilityRow[];
  descriptorOf: (id: number) => string;
}) {
  // Verified marks are keyed by report nonce — a new nonce is a new tree,
  // so stale marks fall away without an effect.
  const [verifiedState, setVerifiedState] = useState<{ nonce: number; map: Record<number, boolean> }>({
    nonce: -1,
    map: {},
  });
  const verified = verifiedState.nonce === report.nonce ? verifiedState.map : {};

  const verify = (row: PorLiabilityRow, index: number) => {
    try {
      const leaf = sha256Tagged("POC-LEAF", leafPayload(row, report.nonce));
      const ok = verifyInclusion(
        leaf,
        {
          siblings: row.inclusion_proof.map((s) => ({ hash: s.hash, right: s.right })),
          leafIndex: index,
        },
        report.root,
      );
      setVerifiedState((prev) => ({
        nonce: report.nonce,
        map: { ...(prev.nonce === report.nonce ? prev.map : {}), [row.subaccount]: ok },
      }));
      if (ok) {
        toast.success("Inclusion proof verified ✓ leaf → root", {
          description: `sub ${row.subaccount} · sha256 × ${row.inclusion_proof.length} sibling folds`,
        });
      } else {
        toast.error("Inclusion proof FAILED", { description: `leaf for sub ${row.subaccount} does not commit to ${report.root.slice(0, 12)}…` });
      }
    } catch (err) {
      toast.error("Verification crashed", { description: err instanceof Error ? err.message : String(err) });
    }
  };

  const copy = (hash: string, id: number) => {
    navigator.clipboard?.writeText(hash).then(
      () => toast.success(`Leaf hash copied — sub ${id}`, { description: `${hash.slice(0, 16)}… (64 hex)` }),
      () => toast.error("Clipboard unavailable"),
    );
  };

  return (
    <section className="panel overflow-hidden" aria-label="Liability leaves">
      <div className="flex items-center justify-between px-3.5 py-2.5 border-b border-hairline">
        <h3 className="text-[9px] uppercase tracking-wider text-muted-foreground">Liabilities — one leaf per subaccount</h3>
        <p className="text-[9.5px] font-mono text-muted-foreground/60">
          {rows.length} leaves · verify runs in-browser
        </p>
      </div>
      <div className="overflow-auto scroll-thin max-h-[520px]">
        <table className="w-full text-[11.5px] border-collapse">
          <thead>
            <tr>
              <Th>Subaccount</Th>
              <Th right>Quote cash</Th>
              <Th>Collateral</Th>
              <Th right>Vault claims</Th>
              <Th>Leaf</Th>
              <Th right>Inclusion</Th>
            </tr>
          </thead>
          <tbody>
            {rows.map((row, i) => {
              const state = verified[row.subaccount];
              return (
                <tr key={row.subaccount} className="hover:bg-muted/30 border-b border-hairline/40">
                  <td className="py-1.5 px-2.5 whitespace-nowrap">
                    <span className="font-mono text-[11px]">{descriptorOf(row.subaccount)}</span>
                    <span className="ml-1.5 text-[9px] text-muted-foreground/50 font-mono">#{row.subaccount}</span>
                  </td>
                  <td className="text-right num px-2.5">{usd(row.quote_cash_minor)}</td>
                  <td className="py-1.5 px-2.5">
                    {row.collateral.length === 0 ? (
                      <span className="text-muted-foreground/40">—</span>
                    ) : (
                      <span className="flex flex-wrap gap-1">
                        {row.collateral.map((c) => (
                          <span
                            key={c.code}
                            className="text-[9.5px] font-mono px-1.5 py-0.5 rounded bg-muted/60 text-muted-foreground"
                            title={`${formatMoney(c.balance_minor, 8)} ${c.code} → haircut value ${usd(c.value_quote_minor)}`}
                          >
                            {c.code} {usdCompact(c.value_quote_minor)}
                          </span>
                        ))}
                      </span>
                    )}
                  </td>
                  <td className="text-right num px-2.5 text-muted-foreground">{usd(row.vault_claims_quote_minor)}</td>
                  <td className="py-1.5 px-2.5 whitespace-nowrap">
                    <span className="inline-flex items-center gap-1.5">
                      <span className="num text-[10px] text-muted-foreground" title={row.leaf_hash}>
                        {row.leaf_hash.slice(0, 10)}…
                      </span>
                      <button
                        onClick={() => copy(row.leaf_hash, row.subaccount)}
                        className="w-4.5 h-4.5 rounded flex items-center justify-center text-muted-foreground/60 hover:text-primary hover:bg-primary/10 transition-colors"
                        aria-label={`Copy leaf hash for sub ${row.subaccount}`}
                      >
                        <Copy className="w-3 h-3" />
                      </button>
                    </span>
                  </td>
                  <td className="text-right px-2.5 whitespace-nowrap">
                    <button
                      onClick={() => verify(row, i)}
                      className={`inline-flex items-center gap-1 h-6 px-2 rounded-md border text-[9.5px] font-mono transition-colors ${
                        state === true
                          ? "border-up/40 text-up bg-up/10"
                          : state === false
                            ? "border-down/40 text-down bg-down/10"
                            : "border-hairline text-muted-foreground hover:text-primary hover:border-primary/40"
                      }`}
                      title="Rebuild the leaf and fold the sibling proof to the root — client-side"
                    >
                      {state === true ? (
                        <>
                          <Check className="w-3 h-3" /> verified
                        </>
                      ) : state === false ? (
                        "mismatch"
                      ) : (
                        "verify"
                      )}
                    </button>
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
    </section>
  );
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

/* ─────────────────────────── merkle tree viz ─────────────────────────── */

interface VizNode {
  hash: string;
  kind: "leaf" | "node" | "proof" | "root";
}

/**
 * Display tree: the first ~4 leaves, folded bottom-up with the same
 * duplicate-odd padding rule, then the remaining siblings borrowed from
 * leaf 0's inclusion proof up to the published root. Every node shown
 * is a real node of the committed tree.
 */
function buildVizLevels(rows: PorLiabilityRow[]): VizNode[][] {
  if (rows.length === 0) return [];
  const k = Math.min(4, rows.length);
  const leaves = rows.slice(0, k).map((r) => r.leaf_hash);
  const levels: VizNode[][] = [leaves.map((h) => ({ hash: h, kind: "leaf" as const }))];

  let cur = leaves;
  let rounds = 0;
  while (cur.length > 1) {
    const next: string[] = [];
    for (let i = 0; i < cur.length; i += 2) {
      const left = cur[i]!;
      const right = i + 1 < cur.length ? cur[i + 1]! : left;
      next.push(sha256Tagged("POC-NODE", `${left}${right}`));
    }
    cur = next;
    levels.push(cur.map((h) => ({ hash: h, kind: "node" as const })));
    rounds++;
  }

  const remaining = rows[0]!.inclusion_proof.slice(rounds);
  if (remaining.length > 0) {
    let acc = cur[0]!;
    levels[levels.length - 1]!.push({ hash: remaining[0]!.hash, kind: "proof" });
    acc = foldNode(acc, remaining[0]!.hash, remaining[0]!.right);
    for (let i = 1; i < remaining.length; i++) {
      levels.push([
        { hash: acc, kind: "node" },
        { hash: remaining[i]!.hash, kind: "proof" },
      ]);
      acc = foldNode(acc, remaining[i]!.hash, remaining[i]!.right);
    }
    levels.push([{ hash: acc, kind: "root" }]);
  } else {
    // The fold already reached the root — mark it.
    const last = levels[levels.length - 1]!;
    last[0] = { hash: last[0]!.hash, kind: "root" };
  }
  return levels;
}

function foldNode(left: string, right: string, rightSibling: boolean): string {
  return rightSibling
    ? sha256Tagged("POC-NODE", `${left}${right}`)
    : sha256Tagged("POC-NODE", `${right}${left}`);
}

function MerkleTreePanel({ report, rows }: { report: PorReport; rows: PorLiabilityRow[] }) {
  const levels = useMemo(() => buildVizLevels(rows), [rows]);
  if (levels.length === 0) return null;

  const W = 620;
  const gap = 66;
  const H = levels.length * gap + 34;
  const pad = 46;

  const xOf = (level: number, j: number): number => {
    const n = levels[level]!.length;
    const span = W - pad * 2;
    return n === 1 ? W / 2 : pad + (span * j) / (n - 1);
  };
  const yOf = (level: number): number => H - 26 - level * gap;

  const computedRoot = levels[levels.length - 1]![0]!.hash;
  const rootMatches = computedRoot === report.root;

  return (
    <section className="panel p-3.5 flex flex-col gap-2" aria-label="Merkle tree">
      <div className="flex items-center justify-between">
        <h3 className="text-[9px] uppercase tracking-wider text-muted-foreground">Merkle tree — first {Math.min(4, rows.length)} leaves</h3>
        <span
          className={`text-[9px] font-mono px-1.5 py-0.5 rounded border ${
            rootMatches ? "text-up border-up/40 bg-up/10" : "text-down border-down/40 bg-down/10"
          }`}
          title={report.root}
        >
          {rootMatches ? "root = report" : "root mismatch"}
        </span>
      </div>
      <svg viewBox={`0 0 ${W} ${H}`} className="w-full" role="img" aria-label="Merkle tree visualization">
        {/* edges: parent (level+1, j) ← children (level, 2j) and (level, 2j+1) */}
        {levels.slice(1).map((_, li) => {
          const level = li; // child level index
          const childCount = levels[level]!.length;
          const parents = levels[level + 1]!;
          return parents.map((_, j) => {
            const px = xOf(level + 1, j);
            const py = yOf(level + 1);
            const kids = [j * 2, j * 2 + 1].map((idx) => levels[level]![Math.min(idx, childCount - 1)]);
            return kids.map((_kid, ki) => {
              const cx = xOf(level, Math.min(j * 2 + ki, childCount - 1));
              const cy = yOf(level);
              return (
                <line
                  key={`${level}-${j}-${ki}`}
                  x1={cx}
                  y1={cy - 9}
                  x2={px}
                  y2={py + 9}
                  className="stroke-hairline"
                  strokeWidth={1}
                />
              );
            });
          });
        })}
        {/* nodes */}
        {levels.map((nodes, level) =>
          nodes.map((node, j) => {
            const x = xOf(level, j);
            const y = yOf(level);
            const label = node.hash.slice(0, 6);
            const circleCls =
              node.kind === "leaf"
                ? "fill-primary/10 stroke-primary"
                : node.kind === "proof"
                  ? "fill-chart-3/10 stroke-chart-3"
                  : node.kind === "root"
                    ? "fill-up/15 stroke-up"
                    : "fill-muted/40 stroke-muted-foreground/60";
            return (
              <g key={`n-${level}-${j}`}>
                <circle
                  cx={x}
                  cy={y}
                  r={9}
                  className={circleCls}
                  strokeWidth={1.5}
                  strokeDasharray={node.kind === "proof" ? "3 2.5" : undefined}
                />
                <text
                  x={x}
                  y={level === 0 ? y + 22 : y - 15}
                  textAnchor="middle"
                  className="fill-muted-foreground font-mono"
                  fontSize={8}
                >
                  {label}
                </text>
              </g>
            );
          }),
        )}
      </svg>
      <div className="flex flex-wrap items-center gap-x-3 gap-y-1 text-[9px] font-mono text-muted-foreground/70">
        <LegendDot className="fill-primary/10 stroke-primary" label="leaf" />
        <LegendDot className="fill-muted/40 stroke-muted-foreground/60" label="POC-NODE" />
        <LegendDot className="fill-chart-3/10 stroke-chart-3" label="proof sibling" dashed />
        <LegendDot className="fill-up/15 stroke-up" label="root" />
        <span className="ml-auto">leaf = sha256("POC-LEAF" ‖ "POC-LEAF" ‖ payload)</span>
      </div>
    </section>
  );
}

function LegendDot({ className, label, dashed }: { className: string; label: string; dashed?: boolean }) {
  return (
    <span className="inline-flex items-center gap-1.5">
      <svg width="12" height="12" viewBox="0 0 12 12" aria-hidden="true">
        <circle cx="6" cy="6" r="4.5" className={className} strokeWidth="1.5" strokeDasharray={dashed ? "2.5 2" : undefined} />
      </svg>
      {label}
    </span>
  );
}

/* ─────────────────────────── explainer ─────────────────────────── */

function ExplainerCard() {
  return (
    <section className="panel p-3.5 bg-panel-2/50" aria-label="How proof of reserves works">
      <h3 className="text-[9px] uppercase tracking-wider text-muted-foreground mb-1.5">How it works</h3>
      <p className="text-[11px] text-muted-foreground leading-relaxed">
        Nonce-bound merkle liability trees over cash + collateral (haircut value) + vault claims. Per-account
        inclusion proofs, a monotonic publication ledger, report commitment hash for on-chain publication.
      </p>
    </section>
  );
}
