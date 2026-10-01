/**
 * Withdrawal pipeline — time-locked, cancellable, manually approved above a
 * threshold, settled through a custody adapter (G-32).
 */

import type { MoneyMinor, WithdrawalRequest, WithdrawalState } from "@perp/types";
import { WITHDRAWAL_POLICY } from "@perp/types";

export class WithdrawalPipeline {
  private requests = new Map<number, WithdrawalRequest>();
  private nextId = 1;
  private dailyTotals = new Map<string, bigint>(); // dayKey → settled total

  list(): WithdrawalRequest[] {
    return [...this.requests.values()].sort((a, b) => a.id - b.id);
  }

  pendingFor(subaccount: number): WithdrawalRequest[] {
    return this.list().filter((r) => r.subaccount === subaccount && r.state === "pending");
  }

  /**
   * Request a withdrawal. `spendable` is the account's available quote cash.
   * Returns [id, needsManual] or an error string.
   */
  request(
    subaccount: number,
    amount: MoneyMinor,
    destination: string,
    spendable: bigint,
    now: number,
  ): [number, boolean] | string {
    if (amount <= 0n) return "invalid_amount";
    if (amount > spendable) return "insufficient_balance";
    if (this.pendingFor(subaccount).length >= WITHDRAWAL_POLICY.max_pending_per_account)
      return "too_many_pending";
    if (amount > WITHDRAWAL_POLICY.per_withdrawal_cap_quote_minor) return "above_per_withdrawal_cap";
    const day = new Date(now).toISOString().slice(0, 10);
    const used = this.dailyTotals.get(day) ?? 0n;
    if (used + amount > WITHDRAWAL_POLICY.daily_quota_quote_minor) return "above_daily_quota";

    const needsManual = amount >= WITHDRAWAL_POLICY.manual_threshold_quote_minor;
    const id = this.nextId++;
    this.requests.set(id, {
      id,
      subaccount,
      amount_quote_minor: amount,
      destination,
      requested_at: now,
      claimable_at: now + WITHDRAWAL_POLICY.settlement_delay_ms,
      state: "pending",
      needs_manual: needsManual,
    });
    return [id, needsManual];
  }

  approve(id: number): string | null {
    const r = this.requests.get(id);
    if (!r) return "unknown withdrawal";
    if (r.state !== "pending") return "not_pending";
    r.state = "approved";
    return null;
  }

  cancel(id: number): string | null {
    const r = this.requests.get(id);
    if (!r) return "unknown withdrawal";
    if (r.state !== "pending" && r.state !== "approved") return "not_pending";
    r.state = "cancelled";
    return null;
  }

  /**
   * Settle everything whose lock has elapsed. `debit` performs the custody
   * move; return false to abort (insufficient funds). Settled amounts count
   * against the daily quota.
   */
  settleDue(
    now: number,
    debit: (subaccount: number, amount: MoneyMinor) => boolean,
  ): { id: number; amount: MoneyMinor }[] {
    const settled: { id: number; amount: MoneyMinor }[] = [];
    for (const r of this.requests.values()) {
      if (r.state !== "pending" && r.state !== "approved") continue;
      if (r.state === "pending" && r.needs_manual) continue; // awaiting operator approval
      if (now < r.claimable_at) continue;
      if (debit(r.subaccount, r.amount_quote_minor)) {
        r.state = "settled";
        settled.push({ id: r.id, amount: r.amount_quote_minor });
        const day = new Date(r.requested_at).toISOString().slice(0, 10);
        this.dailyTotals.set(day, (this.dailyTotals.get(day) ?? 0n) + r.amount_quote_minor);
      }
    }
    return settled;
  }

  /** Cancelled refunds waiting to be credited back. */
  cancelledAmounts(): { id: number; amount: MoneyMinor }[] {
    return this.list()
      .filter((r) => r.state === "cancelled")
      .map((r) => ({ id: r.id, amount: r.amount_quote_minor }));
  }

  stateOf(id: number): WithdrawalState | null {
    return this.requests.get(id)?.state ?? null;
  }
}
