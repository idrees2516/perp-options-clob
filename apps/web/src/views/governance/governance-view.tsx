"use client";

/**
 * Governance — weighted multisig → 24h timelock → 72h grace, with an
 * instant guardian veto (G-33). Proposals mutate the live parameter
 * registry once executed. This is the venue's slow path: every button
 * here is a GovernanceAction shipped to the engine.
 */

import { memo, useCallback, useEffect, useRef, useState } from "react";
import { useVenueStore } from "@/lib/venue-store";
import { getVenueClient } from "@perp/api-client";
import { relTime } from "@/lib/fmt";
import { GOVERNANCE_DEFAULTS, GOVERNANCE_SIGNERS } from "@perp/types";
import type { GovernanceAction, Proposal, ProposalState } from "@perp/types";
import { toast } from "sonner";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Ban, Gavel, Scale, ShieldAlert, Sparkles, Zap } from "lucide-react";

const TOTAL_WEIGHT = Object.values(GOVERNANCE_SIGNERS).reduce((a, b) => a + b, 0);
const PROPOSERS = ["alice.ops", "bob.risk", "carol.treasury"] as const;

const PRESETS: { key: string; value: string; description: string }[] = [
  { key: "funding.rate_cap_bps", value: "100", description: "Cap the per-interval funding rate at 100 bps." },
  { key: "fees.tier0.taker_bps", value: "3", description: "Lower the base taker fee tier to 3 bps." },
  { key: "rewards.budget_hour_quote", value: "1000", description: "Set the liquidity-rewards budget to $1,000 per hour." },
];

export const GovernanceView = memo(function GovernanceView() {
  const snapshot = useVenueStore((s) => s.snapshot);
  const proposals = snapshot?.proposals ?? [];
  const now = snapshot?.meta.now ?? Date.now();

  const pending = proposals.filter((p) => p.state === "pending_approval").length;
  const queued = proposals.filter((p) => p.state === "queued").length;
  const act = useGovernanceActions();

  return (
    <div className="h-full flex flex-col min-h-0">
      {/* toolbar */}
      <div className="h-11 shrink-0 flex items-center gap-2.5 px-3 border-b border-hairline">
        <Scale className="w-3.5 h-3.5 text-primary" />
        <h2 className="text-[12px] font-semibold tracking-tight">Governance</h2>
        <span className="text-[9px] uppercase tracking-wider text-muted-foreground/60 font-mono">
          G-33 · weighted multisig → timelock → grace
        </span>
        <div className="flex-1" />
        <p className="text-[10px] text-muted-foreground/60 font-mono hidden sm:block">
          {proposals.length} proposals · {pending} pending · {queued} queued
        </p>
        <Button
          variant="ghost"
          size="sm"
          className="h-7 text-[10px] text-muted-foreground hover:text-foreground border border-hairline"
          onClick={() => act({ type: "sweep_expiries" }, "Expiry sweep complete")}
        >
          <Sparkles className="w-3 h-3" />
          Sweep expiries
        </Button>
      </div>

      {/* content */}
      <div className="flex-1 min-h-0 overflow-auto scroll-thin p-3">
        <div className="grid grid-cols-1 xl:grid-cols-[330px_minmax(0,1fr)] gap-3 items-start max-w-[1440px]">
          <div className="flex flex-col gap-3">
            <SignersPanel />
            <ProposeForm act={act} />
          </div>
          <div className="flex flex-col gap-3">
            {proposals.length === 0 && (
              <div className="panel p-10 flex flex-col items-center justify-center gap-2 min-h-[240px]">
                <Scale className="w-5 h-5 text-muted-foreground/30" />
                <p className="text-muted-foreground/50 text-[11px]">
                  No proposals yet — the multisig is quiet.
                </p>
              </div>
            )}
            {[...proposals].reverse().map((p) => (
              <ProposalCard key={p.id} p={p} now={now} act={act} />
            ))}
            <ExplainerCard />
          </div>
        </div>
      </div>
    </div>
  );
});

/* ─────────────────────────── action plumbing ─────────────────────────── */

/**
 * The transport ships GovernanceActions over the control channel and the
 * engine answers asynchronously: rejections surface on the error channel
 * (store.lastError) within milliseconds, successes only show up in the next
 * snapshot. So we hold the success toast for a beat and let any error win.
 */
function useGovernanceActions() {
  const lastError = useVenueStore((s) => s.lastError);
  const markErrorSeen = useVenueStore((s) => s.markErrorSeen);
  const errorCountRef = useRef(0);

  useEffect(() => {
    if (!lastError || !lastError.startsWith("governance:")) return;
    errorCountRef.current += 1;
    toast.error(`Governance: ${lastError.slice("governance:".length).trim()}`, {
      description: "GovernanceAction rejected by the engine",
    });
    markErrorSeen();
  }, [lastError, markErrorSeen]);

  return useCallback((action: GovernanceAction, okMessage: string) => {
    getVenueClient().send({ type: "governance", action });
    const errorsBefore = errorCountRef.current;
    window.setTimeout(() => {
      if (errorCountRef.current !== errorsBefore) return; // rejection already toasted
      toast.success(okMessage, { description: `GovernanceAction::${action.type} → engine` });
    }, 400);
  }, []);
}

/* ─────────────────────────── signers panel ─────────────────────────── */

function SignersPanel() {
  return (
    <section className="panel p-3.5" aria-label="Multisig signers">
      <h3 className="text-[9px] uppercase tracking-wider text-muted-foreground mb-2.5">Signers — weighted multisig</h3>
      <div className="flex flex-col gap-1.5">
        {Object.entries(GOVERNANCE_SIGNERS).map(([name, weight]) => (
          <div
            key={name}
            className="flex items-center justify-between rounded-md px-2 py-1.5 bg-panel-2 border border-hairline/60"
          >
            <span className="font-mono text-[11px]">{name}</span>
            {weight > 0 ? (
              <span className="num text-[10px] text-primary">weight ×{weight}</span>
            ) : (
              <span className="text-[9px] uppercase tracking-wider text-chart-3">veto only</span>
            )}
          </div>
        ))}
      </div>
      <p className="mt-2.5 text-[10px] text-muted-foreground font-mono text-center">
        approval threshold: <span className="text-primary">{GOVERNANCE_DEFAULTS.approval_threshold_weight}</span> of{" "}
        {TOTAL_WEIGHT} weight
      </p>
    </section>
  );
}

/* ─────────────────────────── propose form ─────────────────────────── */

function ProposeForm({ act }: { act: (action: GovernanceAction, ok: string) => void }) {
  const [key, setKey] = useState("fees.tier0.taker_bps");
  const [value, setValue] = useState("3");
  const [description, setDescription] = useState("Lower the base taker fee tier to 3 bps.");
  const [proposer, setProposer] = useState<(typeof PROPOSERS)[number]>("alice.ops");

  const submit = () => {
    if (!key.trim()) {
      toast.error("Parameter key is required", { description: "e.g. fees.tier0.taker_bps" });
      return;
    }
    if (!/^\d+$/.test(value.trim())) {
      toast.error("Value must be a non-negative integer", { description: "Parameters are u128 on the wire — parsed as bigint" });
      return;
    }
    if (!description.trim()) {
      toast.error("Description is required", { description: "Human-readable intent is journaled with the proposal" });
      return;
    }
    act(
      { type: "propose", key: key.trim(), value: BigInt(value.trim()), description: description.trim(), proposer },
      `Proposal published — ${key.trim()} = ${value.trim()}`,
    );
  };

  return (
    <section className="panel p-3.5" aria-label="Propose parameter change">
      <h3 className="text-[9px] uppercase tracking-wider text-muted-foreground mb-2.5">Propose — parameter change</h3>

      <div className="flex flex-wrap gap-1.5 mb-3">
        {PRESETS.map((p) => (
          <button
            key={p.key}
            onClick={() => {
              setKey(p.key);
              setValue(p.value);
              setDescription(p.description);
            }}
            className="text-[9px] font-mono px-2 py-1 rounded-md border border-hairline text-muted-foreground hover:text-primary hover:border-primary/40 transition-colors"
            title={p.description}
          >
            {p.key} → {p.value}
          </button>
        ))}
      </div>

      <div className="flex flex-col gap-2">
        <label className="flex flex-col gap-1">
          <span className="text-[9px] uppercase tracking-wider text-muted-foreground">Key</span>
          <Input
            value={key}
            onChange={(e) => setKey(e.target.value)}
            placeholder="fees.tier0.taker_bps"
            className="h-8 text-[11px] font-mono bg-transparent border-hairline"
          />
        </label>
        <div className="grid grid-cols-[1fr_140px] gap-2">
          <label className="flex flex-col gap-1">
            <span className="text-[9px] uppercase tracking-wider text-muted-foreground">Value (integer → bigint)</span>
            <Input
              value={value}
              onChange={(e) => setValue(e.target.value)}
              placeholder="3"
              inputMode="numeric"
              className="h-8 text-[11px] font-mono bg-transparent border-hairline num"
            />
          </label>
          <label className="flex flex-col gap-1">
            <span className="text-[9px] uppercase tracking-wider text-muted-foreground">Proposer</span>
            <Select value={proposer} onValueChange={(v) => setProposer(v as (typeof PROPOSERS)[number])}>
              <SelectTrigger size="sm" className="h-8 text-[11px] font-mono bg-transparent border-hairline w-full">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {PROPOSERS.map((p) => (
                  <SelectItem key={p} value={p} className="text-[11px] font-mono">
                    {p}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </label>
        </div>
        <label className="flex flex-col gap-1">
          <span className="text-[9px] uppercase tracking-wider text-muted-foreground">Description</span>
          <Textarea
            value={description}
            onChange={(e) => setDescription(e.target.value)}
            placeholder="What this parameter change does and why."
            className="min-h-[56px] text-[11px] bg-transparent border-hairline resize-none"
          />
        </label>
        <Button
          size="sm"
          className="h-8 text-[11px] font-semibold"
          onClick={submit}
        >
          <Gavel className="w-3 h-3" />
          Submit proposal
        </Button>
      </div>
    </section>
  );
}

/* ─────────────────────────── proposal cards ─────────────────────────── */

function ProposalCard({
  p,
  now,
  act,
}: {
  p: Proposal;
  now: number;
  act: (action: GovernanceAction, ok: string) => void;
}) {
  const cfg = GOVERNANCE_DEFAULTS;
  const approvalWeight = Object.values(p.approvals).reduce((a, b) => a + b, 0);
  const threshold = cfg.approval_threshold_weight;

  const timelockP =
    p.queued_at != null ? clamp01((now - p.queued_at) / cfg.timelock_ms) : 0;
  const graceP =
    p.queued_at != null
      ? clamp01((now - p.queued_at - cfg.timelock_ms) / cfg.grace_ms)
      : 0;
  const executable = p.queued_at != null && now >= p.queued_at + cfg.timelock_ms;

  const eligibleSigners = Object.entries(GOVERNANCE_SIGNERS).filter(
    ([name, w]) => w > 0 && p.approvals[name] == null,
  );
  const live = p.state === "pending_approval" || p.state === "queued";

  return (
    <article className="panel p-3.5 flex flex-col gap-2.5">
      {/* header */}
      <div className="flex items-center gap-2 flex-wrap">
        <span className="text-[10px] font-mono text-muted-foreground/60">#{p.id}</span>
        <span className="font-mono text-[12px] font-medium">{p.key}</span>
        <span className="num text-[11px] text-chart-3">= {p.value.toString()}</span>
        <div className="flex-1" />
        <StateBadge state={p.state} />
      </div>

      <p className="text-[11px] text-muted-foreground leading-relaxed">{p.description}</p>

      {/* meta grid */}
      <div className="grid grid-cols-2 sm:grid-cols-4 gap-x-4 gap-y-1.5 text-[10px]">
        <Meta label="proposer">
          <span className="font-mono">{p.proposer}</span>
        </Meta>
        <Meta label="created">{relTime(p.created_at, now)}</Meta>
        {p.executed_at != null && <Meta label="executed">{relTime(p.executed_at, now)}</Meta>}
        {p.queued_at != null && (
          <Meta label="queued">{relTime(p.queued_at, now)}</Meta>
        )}
      </div>

      {/* approvals */}
      {live && (
        <div className="flex items-center gap-1.5 flex-wrap">
          <span className="text-[9px] uppercase tracking-wider text-muted-foreground/70 mr-1">
            approvals {approvalWeight}/{threshold}w
          </span>
          {Object.entries(p.approvals).map(([name, w]) => (
            <span
              key={name}
              className="text-[9.5px] font-mono px-1.5 py-0.5 rounded-md bg-primary/10 text-primary border border-primary/25"
            >
              {name} ×{w}
            </span>
          ))}
          {Object.keys(p.approvals).length === 0 && (
            <span className="text-[10px] text-muted-foreground/40 font-mono">none yet</span>
          )}
          {p.state === "pending_approval" && (
            <div className="w-24 h-1 rounded-full bg-muted overflow-hidden ml-1">
              <div
                className="h-full bg-primary transition-all"
                style={{ width: `${Math.min(100, (approvalWeight / threshold) * 100)}%` }}
              />
            </div>
          )}
        </div>
      )}

      {/* timelock + grace bars */}
      {p.state === "queued" && p.queued_at != null && (
        <div className="flex flex-col gap-1.5">
          <ProgressRow
            label="timelock 24h"
            progress={timelockP}
            tail={relTime(p.queued_at + cfg.timelock_ms, now)}
            className="bg-primary"
          />
          <ProgressRow
            label="grace 72h"
            progress={graceP}
            tail={relTime(p.queued_at + cfg.timelock_ms + cfg.grace_ms, now)}
            className="bg-chart-3"
          />
        </div>
      )}

      {/* actions */}
      {live && (
        <div className="flex items-center gap-1.5 flex-wrap pt-0.5 border-t border-hairline/50 pt-2.5">
          {p.state === "pending_approval" &&
            eligibleSigners.map(([name, w]) => (
              <Button
                key={name}
                variant="outline"
                size="sm"
                className="h-6.5 text-[10px] font-mono"
                onClick={() =>
                  act({ type: "approve", proposal_id: p.id, signer: name }, `Approved as ${name} (+${w} weight)`)
                }
              >
                Approve as {name}
              </Button>
            ))}
          {p.state === "queued" && (
            <Button
              size="sm"
              disabled={!executable}
              className="h-6.5 text-[10px]"
              onClick={() => act({ type: "execute", proposal_id: p.id }, `Executed — ${p.key} is live`)}
              title={executable ? "Apply the parameter change" : "Timelock still running"}
            >
              <Zap className="w-3 h-3" />
              {executable ? "Execute" : `Timelock ${relTime(p.queued_at! + cfg.timelock_ms, now)}`}
            </Button>
          )}
          <div className="flex-1" />
          <Button
            variant="ghost"
            size="sm"
            className="h-6.5 text-[10px] text-muted-foreground hover:text-down"
            onClick={() =>
              act({ type: "cancel", proposal_id: p.id, by: p.proposer }, `Proposal #${p.id} cancelled by ${p.proposer}`)
            }
            title={`Cancel as ${p.proposer} (the proposer)`}
          >
            <Ban className="w-3 h-3" />
            Cancel
          </Button>
          <Button
            variant="ghost"
            size="sm"
            className="h-6.5 text-[10px] text-muted-foreground hover:text-chart-3"
            onClick={() => act({ type: "veto", proposal_id: p.id, by: "guardian" }, `Proposal #${p.id} vetoed by guardian`)}
            title="Guardian veto — instantaneous, no timelock"
          >
            <ShieldAlert className="w-3 h-3" />
            Veto
          </Button>
        </div>
      )}
    </article>
  );
}

function StateBadge({ state }: { state: ProposalState }) {
  switch (state) {
    case "pending_approval":
      return (
        <Badge className="text-chart-3 border-chart-3/40 bg-chart-3/10">
          <span className="w-1.5 h-1.5 rounded-full bg-chart-3 pulse-dot" />
          pending approval
        </Badge>
      );
    case "queued":
      return (
        <Badge className="text-primary border-primary/40 bg-primary/10">
          <span className="w-1.5 h-1.5 rounded-full bg-primary pulse-dot" />
          queued · timelock
        </Badge>
      );
    case "executed":
      return <Badge className="text-up border-up/40 bg-up/10">executed</Badge>;
    default:
      return <Badge className="text-muted-foreground border-hairline bg-muted/30">{state}</Badge>;
  }
}

function Badge({ children, className }: { children: React.ReactNode; className?: string }) {
  return (
    <span
      className={`inline-flex items-center gap-1.5 text-[9px] uppercase tracking-wider font-mono px-2 py-0.5 rounded-md border ${className ?? ""}`}
    >
      {children}
    </span>
  );
}

function Meta({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="flex items-baseline gap-1.5 min-w-0">
      <span className="uppercase tracking-wider text-muted-foreground/50 text-[9px]">{label}</span>
      <span className="num text-muted-foreground truncate">{children}</span>
    </div>
  );
}

function ProgressRow({
  label,
  progress,
  tail,
  className,
}: {
  label: string;
  progress: number;
  tail: string;
  className: string;
}) {
  return (
    <div className="flex items-center gap-2">
      <span className="text-[9px] uppercase tracking-wider text-muted-foreground/70 w-[86px] shrink-0">{label}</span>
      <div className="flex-1 h-1.5 rounded-full bg-muted overflow-hidden">
        <div className={`h-full rounded-full transition-all ${className}`} style={{ width: `${progress * 100}%` }} />
      </div>
      <span className="num text-[9.5px] text-muted-foreground/60 w-[64px] text-right shrink-0">{tail}</span>
    </div>
  );
}

function ExplainerCard() {
  return (
    <section className="panel p-3.5 bg-panel-2/50" aria-label="How governance works">
      <h3 className="text-[9px] uppercase tracking-wider text-muted-foreground mb-1.5">How it works</h3>
      <p className="text-[11px] text-muted-foreground leading-relaxed">
        Weighted multisig → 24h timelock → 72h grace. The guardian vetoes instantly. Executed values land in
        the live parameter registry.
      </p>
    </section>
  );
}

function clamp01(x: number): number {
  return Math.max(0, Math.min(1, x));
}
