"use client";

/**
 * Vaults — the LP underwriter desk (G-16).
 *
 * The insurance share of every routed fee flows through these vaults
 * before the fund: each vault takes its configured bps of the allocation
 * (ascending vault-id order), the remainder lands in the insurance fund.
 * Subscriptions and redemptions queue and settle at the next epoch's NAV.
 */

import { memo, useMemo, useState } from "react";
import { useVenueStore } from "@/lib/venue-store";
import { getVenueClient } from "@perp/api-client";
import { usd, pct, simTime } from "@/lib/fmt";
import { formatMoney, tryParseMoney } from "@perp/types";
import type { VaultPosition, VaultState } from "@perp/types";
import { toast } from "sonner";
import { cn } from "@/lib/utils";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Vault, Info, ArrowDownToLine, ArrowUpFromLine } from "lucide-react";

export const VaultsView = memo(function VaultsView() {
  const snapshot = useVenueStore((s) => s.snapshot);
  const journal = useVenueStore((s) => s.journal);
  const account = useVenueStore((s) => s.activeAccount);

  const now = snapshot?.meta.now ?? Date.now();
  const vaults = snapshot?.vaults ?? [];
  const positions = snapshot?.vaultPositions[account] ?? [];
  const accountName =
    snapshot?.accountDescriptors.find((d) => d.id === account)?.name ?? `Sub ${account}`;

  /** Epoch settlements — newest first, capped at 40 rows. */
  const epochRows = useMemo(() => {
    const rows: {
      vault_id: number;
      epoch: number;
      nav: bigint;
      sub: bigint;
      red: bigint;
      ts: number;
    }[] = [];
    for (let i = journal.length - 1; i >= 0 && rows.length < 40; i--) {
      const ev = journal[i].event;
      if (ev.type !== "vault_epoch_settled") continue;
      rows.push({
        vault_id: ev.payload.vault_id,
        epoch: ev.payload.epoch,
        nav: ev.payload.nav_per_share_quote_minor,
        sub: ev.payload.subscribed_quote_minor,
        red: ev.payload.redeemed_quote_minor,
        ts: ev.payload.ts,
      });
    }
    return rows;
  }, [journal]);

  return (
    <div className="h-full flex flex-col min-h-0">
      {/* Toolbar */}
      <div className="h-11 shrink-0 flex items-center gap-2.5 px-3 border-b border-hairline">
        <Vault className="w-4 h-4 text-primary" />
        <h1 className="text-[13px] font-semibold">Underwriter Vaults</h1>
        <span className="text-[8px] font-mono px-1.5 py-0.5 rounded border border-primary/30 bg-primary/10 text-primary">
          G-16
        </span>
        <div className="flex-1" />
        <p className="text-[10px] text-muted-foreground/60 font-mono hidden sm:block truncate">
          acting as {accountName} · flows queue for the next epoch
        </p>
      </div>

      <div className="flex-1 min-h-0 overflow-auto scroll-thin p-3">
        {/* Vault cards */}
        <div
          className={cn(
            "grid gap-3 items-start",
            vaults.length > 1 && "lg:grid-cols-2",
          )}
        >
          {vaults.map((v) => (
            <VaultCard
              key={v.vault_id}
              vault={v}
              position={positions.find((p) => p.vault_id === v.vault_id) ?? null}
              account={account}
              now={now}
            />
          ))}
          {vaults.length === 0 && (
            <div className="panel p-10 flex items-center justify-center">
              <p className="text-muted-foreground/50 text-[11px]">
                No vaults open — the underwriter desk is dark.
              </p>
            </div>
          )}
        </div>

        {/* Epoch activity */}
        <section className="panel mt-3" aria-label="Vault epoch activity">
          <header className="h-9 px-3 flex items-center justify-between border-b border-hairline">
            <h2 className="text-[10px] uppercase tracking-wider text-muted-foreground font-medium">
              Epoch activity
            </h2>
            <span className="text-[9px] font-mono text-muted-foreground/50">
              Event::VaultEpochSettled · newest first · cap 40
            </span>
          </header>
          <div className="max-h-80 overflow-auto scroll-thin">
            <table className="w-full text-[11.5px] border-collapse">
              <thead>
                <tr>
                  <Th>Vault</Th>
                  <Th right>Epoch</Th>
                  <Th right>NAV / share</Th>
                  <Th right>Subscribed</Th>
                  <Th right>Redeemed</Th>
                  <Th right>Settled</Th>
                </tr>
              </thead>
              <tbody>
                {epochRows.length === 0 && (
                  <EmptyRow cols={6} text="No epochs settled yet — the vaults are waiting on their first flows." />
                )}
                {epochRows.map((r, i) => (
                  <tr key={`${r.vault_id}-${r.epoch}-${i}`} className="hover:bg-muted/30 border-b border-hairline/40">
                    <Td className="font-mono text-[11px]">Vault {r.vault_id}</Td>
                    <Td right className="text-muted-foreground">{r.epoch.toLocaleString()}</Td>
                    <Td right>{usd(r.nav)}</Td>
                    <Td right className={r.sub > 0n ? "text-up" : "text-muted-foreground"}>{usd(r.sub)}</Td>
                    <Td right className={r.red > 0n ? "text-chart-3" : "text-muted-foreground"}>{usd(r.red)}</Td>
                    <Td right className="text-muted-foreground/60 text-[10px] font-mono">{simTime(r.ts)}</Td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </section>

        {/* Explainer */}
        <section className="panel mt-3 p-4" aria-label="How vaults work">
          <div className="flex items-center gap-2 mb-1.5">
            <Info className="w-3.5 h-3.5 text-primary" />
            <h2 className="text-[10px] uppercase tracking-wider text-muted-foreground font-medium">
              How underwriter vaults work
            </h2>
          </div>
          <p className="text-[11px] leading-relaxed text-muted-foreground">
            The insurance share of every routed fee flows through LP underwriter vaults before the
            fund — each vault takes its configured bps of the allocation (ascending vault-id order),
            remainder to the fund. Subscriptions settle at the next epoch&apos;s NAV.
          </p>
        </section>
      </div>
    </div>
  );
});

/* ═════════════════════════ vault card ═════════════════════════ */

function VaultCard({
  vault,
  position,
  account,
  now,
}: {
  vault: VaultState;
  position: VaultPosition | null;
  account: number;
  now: number;
}) {
  const [amount, setAmount] = useState("");
  const [shares, setShares] = useState("");

  const sharePct = (vault.revenue_share_bps / 100).toFixed(0);
  const gain = Number(((vault.nav_per_share_quote_minor - 1_00n) * 10_000n) / 1_00n) / 100;

  const subscribe = () => {
    const minor = tryParseMoney(amount, 2);
    if (minor == null || minor <= 0n) {
      toast.error("Invalid amount", { description: "Enter a positive USD amount, e.g. 5000." });
      return;
    }
    getVenueClient().send({
      type: "command",
      command: {
        type: "vault_subscribe",
        vault_id: vault.vault_id,
        subaccount: account,
        amount_quote_minor: minor,
        now,
      },
    });
    toast.success(`Subscribed ${usd(minor)} to Vault ${vault.vault_id}`, {
      description: "Command::VaultSubscribe → Event::VaultQueued (settles at next epoch)",
    });
    setAmount("");
  };

  const redeem = () => {
    const t = shares.trim();
    if (!/^\d+$/.test(t)) {
      toast.error("Invalid share count", { description: "Shares are whole integers — e.g. 25000." });
      return;
    }
    const sh = BigInt(t);
    if (sh <= 0n) {
      toast.error("Invalid share count", { description: "Enter at least one share to redeem." });
      return;
    }
    if (position && sh > position.shares) {
      toast.error("Above your position", {
        description: `You hold ${formatMoney(position.shares, 0)} shares in Vault ${vault.vault_id}.`,
      });
      return;
    }
    getVenueClient().send({
      type: "command",
      command: {
        type: "vault_redeem",
        vault_id: vault.vault_id,
        subaccount: account,
        shares: sh,
        now,
      },
    });
    toast.success(`Redeem ${sh.toLocaleString("en-US")} shares queued`, {
      description: "Command::VaultRedeem → Event::VaultQueued (settles at next epoch)",
    });
    setShares("");
  };

  return (
    <section className="panel p-4 flex flex-col gap-3" aria-label={`Vault ${vault.vault_id}`}>
      {/* Header */}
      <div className="flex items-start justify-between gap-2">
        <div className="flex items-center gap-2.5 min-w-0">
          <div className="w-7 h-7 rounded-lg bg-primary/10 border border-primary/25 flex items-center justify-center shrink-0">
            <Vault className="w-3.5 h-3.5 text-primary" />
          </div>
          <div className="min-w-0">
            <h3 className="text-[12.5px] font-semibold leading-none">Vault {vault.vault_id}</h3>
            <p className="text-[9px] text-muted-foreground mt-1 truncate">
              LP underwriter · first-loss capital behind the insurance fund
            </p>
          </div>
        </div>
        <div className="flex items-center gap-1.5 shrink-0">
          <span className="text-[9px] font-mono px-1.5 py-0.5 rounded border border-primary/30 bg-primary/10 text-primary">
            {sharePct}% of insurance
          </span>
          <span className="text-[9px] font-mono px-1.5 py-0.5 rounded border border-hairline bg-muted/50 text-muted-foreground">
            epoch {vault.epoch.toLocaleString()}
          </span>
        </div>
      </div>

      {/* NAV/share gauge vs genesis baseline */}
      <div>
        <div className="flex items-end justify-between mb-1">
          <span className="text-[9px] uppercase tracking-wider text-muted-foreground">
            NAV / share vs $1.00 genesis
          </span>
          <span className={cn("num text-[11px] font-semibold", gain >= 0 ? "text-up" : "text-down")}>
            {pct(gain)} since genesis
          </span>
        </div>
        <NavGauge navPerShare={vault.nav_per_share_quote_minor} />
        <div className="flex justify-between text-[8px] font-mono text-muted-foreground/50 mt-0.5">
          <span>0.50×</span>
          <span>1.00× genesis</span>
          <span>2.00×</span>
        </div>
      </div>

      {/* Stats */}
      <div className="grid grid-cols-3 gap-x-3 gap-y-2.5">
        <Stat label="NAV" value={usd(vault.nav_quote_minor)} />
        <Stat label="NAV / share" value={usd(vault.nav_per_share_quote_minor)} />
        <Stat label="Total shares" value={formatMoney(vault.total_shares, 0)} />
        <Stat
          label="Pending subs"
          value={usd(vault.pending_subscriptions_quote_minor)}
          accent={vault.pending_subscriptions_quote_minor > 0n ? "up" : undefined}
        />
        <Stat
          label="Pending redeems"
          value={`${formatMoney(vault.pending_redemptions_shares, 0)} sh`}
          accent={vault.pending_redemptions_shares > 0n ? "amber" : undefined}
        />
        <Stat label="Lifetime credit" value={usd(vault.lifetime_credit_quote_minor)} accent="amber" />
      </div>

      {/* Your position + controls */}
      <div className="rounded-lg bg-muted/25 border border-hairline p-2.5 space-y-2.5">
        <div className="flex items-center justify-between gap-2">
          <span className="text-[9px] uppercase tracking-wider text-muted-foreground">Your position</span>
          {position ? (
            <span className="num text-[10.5px] text-muted-foreground truncate">
              {formatMoney(position.shares, 0)} sh · {usd(position.value_quote_minor)} · last claim{" "}
              {usd(position.last_claim_quote_minor)}
            </span>
          ) : (
            <span className="text-[10px] text-muted-foreground/50">none yet</span>
          )}
        </div>
        <div className="grid grid-cols-1 sm:grid-cols-2 gap-2">
          {/* Subscribe */}
          <div className="space-y-1">
            <label
              htmlFor={`sub-${vault.vault_id}`}
              className="text-[9px] uppercase tracking-wider text-muted-foreground/70 block"
            >
              Subscribe (USD)
            </label>
            <div className="flex gap-1.5">
              <div className="relative flex-1 min-w-0">
                <span className="absolute left-2.5 top-1/2 -translate-y-1/2 text-[10px] text-muted-foreground font-mono pointer-events-none">
                  $
                </span>
                <Input
                  id={`sub-${vault.vault_id}`}
                  value={amount}
                  onChange={(e) => setAmount(e.target.value)}
                  inputMode="decimal"
                  placeholder="5,000"
                  className="h-8 pl-6 num text-[11.5px]"
                />
              </div>
              <Button
                size="sm"
                onClick={subscribe}
                className="h-8 bg-up/90 hover:bg-up text-[#052018] text-[11px] font-semibold shrink-0"
              >
                <ArrowDownToLine className="w-3 h-3 mr-1" /> Subscribe
              </Button>
            </div>
          </div>
          {/* Redeem */}
          <div className="space-y-1">
            <label
              htmlFor={`red-${vault.vault_id}`}
              className="text-[9px] uppercase tracking-wider text-muted-foreground/70 block"
            >
              Redeem (shares)
            </label>
            <div className="flex gap-1.5">
              <div className="relative flex-1 min-w-0">
                <Input
                  id={`red-${vault.vault_id}`}
                  value={shares}
                  onChange={(e) => setShares(e.target.value)}
                  inputMode="numeric"
                  placeholder={position ? formatMoney(position.shares, 0) : "0"}
                  className="h-8 num text-[11.5px]"
                />
                {position && position.shares > 0n && (
                  <button
                    onClick={() => setShares(formatMoney(position.shares, 0))}
                    className="absolute right-2 top-1/2 -translate-y-1/2 text-[9px] text-primary hover:underline"
                    aria-label="Fill maximum shares"
                  >
                    max
                  </button>
                )}
              </div>
              <Button
                size="sm"
                variant="outline"
                onClick={redeem}
                className="h-8 text-[11px] font-semibold shrink-0"
              >
                <ArrowUpFromLine className="w-3 h-3 mr-1" /> Redeem
              </Button>
            </div>
          </div>
        </div>
      </div>
    </section>
  );
}

/* ═════════════════════════ NAV gauge ═════════════════════════ */

/** Horizontal gauge of nav_per_share against the 1_00n genesis baseline (0.5×–2× range). */
function NavGauge({ navPerShare }: { navPerShare: bigint }) {
  const lo = 50n; // 0.50× genesis
  const hi = 200n; // 2.00× genesis
  const clamped = navPerShare < lo ? lo : navPerShare > hi ? hi : navPerShare;
  const span = Number(hi - lo);
  const x = (v: bigint) => (Number(v - lo) / span) * 100;
  const val = x(clamped);
  const base = x(1_00n);
  const above = navPerShare >= 1_00n;
  const left = Math.min(base, val);
  const width = Math.abs(val - base);

  return (
    <svg
      viewBox="0 0 100 18"
      preserveAspectRatio="none"
      className="w-full h-9"
      role="img"
      aria-label={`NAV per share ${usd(navPerShare)} against the $1.00 genesis baseline`}
    >
      <rect x="0" y="7" width="100" height="4" rx="2" className="fill-muted" />
      {width > 0.15 && (
        <rect
          x={left}
          y="7"
          width={width}
          height="4"
          className={above ? "fill-up" : "fill-down"}
          opacity="0.85"
        />
      )}
      {/* Genesis baseline marker */}
      <rect x={base - 0.45} y="3.5" width="0.9" height="11" rx="0.45" className="fill-chart-3" />
      {/* Value needle */}
      <rect x={Math.max(0, val - 0.55)} y="3" width="1.1" height="12" rx="0.55" className="fill-primary" />
    </svg>
  );
}

/* ═════════════════════════ locals ═════════════════════════ */

function Stat({ label, value, accent }: { label: string; value: string; accent?: "up" | "amber" }) {
  return (
    <div className="min-w-0">
      <p className="text-[8.5px] uppercase tracking-wider text-muted-foreground/70 truncate">{label}</p>
      <p
        className={cn(
          "num text-[12px] font-semibold mt-0.5 truncate",
          accent === "up" && "text-up",
          accent === "amber" && "text-chart-3",
        )}
      >
        {value}
      </p>
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
