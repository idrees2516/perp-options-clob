/**
 * Governance — weighted multisig → timelock → grace with guardian veto (G-33).
 * Executed proposals land in a live parameter registry the engine reads.
 */

import type { GovernanceConfig, GovernanceEvent, GovernanceAction, Proposal } from "@perp/types";
import { GOVERNANCE_DEFAULTS, GOVERNANCE_SIGNERS } from "@perp/types";

export class GovernanceEngine {
  readonly config: GovernanceConfig;
  private proposals = new Map<number, Proposal>();
  private nextId = 1;
  /** Live parameter registry (governance is the only writer). */
  readonly params = new Map<string, bigint>();
  events: GovernanceEvent[] = [];

  constructor(config: Partial<GovernanceConfig> = {}) {
    this.config = { ...GOVERNANCE_DEFAULTS, ...config };
  }

  list(): Proposal[] {
    return [...this.proposals.values()].sort((a, b) => a.id - b.id);
  }

  get(id: number): Proposal | undefined {
    return this.proposals.get(id);
  }

  /** Returns error string, or null on success. */
  act(action: GovernanceAction, now: number): string | null {
    switch (action.type) {
      case "propose": {
        const id = this.nextId++;
        this.proposals.set(id, {
          id,
          key: action.key,
          value: action.value,
          description: action.description,
          proposer: action.proposer,
          state: "pending_approval",
          approvals: {},
          queued_at: null,
          executed_at: null,
          created_at: now,
        });
        this.events.push({ type: "proposed", id });
        return null;
      }
      case "approve": {
        const p = this.proposals.get(action.proposal_id);
        if (!p) return "unknown proposal";
        const weight = GOVERNANCE_SIGNERS[action.signer];
        if (weight == null || weight === 0) return "unknown_signer";
        if (p.state !== "pending_approval") return "wrong_state";
        if (p.approvals[action.signer] != null) return "already_approved";
        p.approvals[action.signer] = weight;
        this.events.push({ type: "approved", id: p.id, signer: action.signer, weight });
        const total = Object.values(p.approvals).reduce((a, b) => a + b, 0);
        if (total >= this.config.approval_threshold_weight) {
          p.state = "queued";
          p.queued_at = now;
          this.events.push({ type: "queued", id: p.id, eta: now + this.config.timelock_ms });
        }
        return null;
      }
      case "queue":
        return "wrong_state"; // queueing is automatic on threshold
      case "execute": {
        const p = this.proposals.get(action.proposal_id);
        if (!p) return "unknown proposal";
        if (p.state !== "queued") return "wrong_state";
        if (p.queued_at == null) return "wrong_state";
        if (now < p.queued_at + this.config.timelock_ms) return "too_early";
        if (now > p.queued_at + this.config.timelock_ms + this.config.grace_ms) {
          p.state = "expired";
          this.events.push({ type: "expired", id: p.id });
          return "expired";
        }
        p.state = "executed";
        p.executed_at = now;
        this.params.set(p.key, p.value);
        this.events.push({ type: "executed", id: p.id, key: p.key, value: p.value });
        return null;
      }
      case "cancel": {
        const p = this.proposals.get(action.proposal_id);
        if (!p) return "unknown proposal";
        if (p.state === "executed" || p.state === "expired" || p.state === "cancelled") return "wrong_state";
        const isProposer = action.by === p.proposer;
        const isGuardian = action.by === this.config.guardian;
        if (!isProposer && !isGuardian) return "not authorized";
        p.state = "cancelled";
        this.events.push({ type: "cancelled", id: p.id });
        return null;
      }
      case "veto": {
        const p = this.proposals.get(action.proposal_id);
        if (!p) return "unknown proposal";
        if (action.by !== this.config.guardian) return "not_guardian";
        if (p.state === "executed" || p.state === "expired" || p.state === "cancelled") return "wrong_state";
        p.state = "cancelled";
        this.events.push({ type: "cancelled", id: p.id });
        return null;
      }
      case "sweep_expiries": {
        for (const p of this.proposals.values()) {
          if (
            p.state === "queued" &&
            p.queued_at != null &&
            now > p.queued_at + this.config.timelock_ms + this.config.grace_ms
          ) {
            p.state = "expired";
            this.events.push({ type: "expired", id: p.id });
          }
        }
        return null;
      }
    }
  }

  /** Timelock + grace progress for a queued proposal (0..1 each). */
  progressOf(p: Proposal, now: number): { timelock: number; grace: number } {
    if (p.queued_at == null) return { timelock: 0, grace: 0 };
    const t = (now - p.queued_at) / this.config.timelock_ms;
    const g = (now - p.queued_at - this.config.timelock_ms) / this.config.grace_ms;
    return { timelock: clamp01(t), grace: clamp01(g) };
  }
}

function clamp01(x: number): number {
  return Math.max(0, Math.min(1, x));
}
