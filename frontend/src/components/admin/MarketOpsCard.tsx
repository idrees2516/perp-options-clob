'use client';

/**
 * MarketOpsCard — oracle mark feed + halt/resume (contract §2.4).
 * Halt/resume are confirmed in a dialog (market-sensitive actions). Status
 * badges come from the polled /admin/audit (conservation, per-shard state
 * hash / book orders) plus the live per-underlying halted/fast state from
 * the WS store.
 */
import { useState } from 'react';
import { useTerminal } from '@/providers/TerminalProvider';
import { useStoreSection } from '@/lib/store';
import { usePoll } from '@/hooks/usePoll';
import { useToast } from '@/components/Toast';
import { fmtInt, fmtTicks, parsePriceTicks, parseUInt } from '@/lib/format';
import { describeError } from '@/lib/messages';
import { Panel } from '@/components/Panel';
import { ConfirmDialog } from '@/components/ConfirmDialog';
import { Field } from '@/components/Field';
import { Badge } from '@/components/StatusDot';
import { EmptyHint } from '@/components/EmptyHint';

export function MarketOpsCard() {
  const { client, store } = useTerminal();
  const { push } = useToast();
  useStoreSection(store, 'underlying');
  useStoreSection(store, 'conn');

  const { data: audit, error: auditError, refresh: refreshAudit } = usePoll(
    () => client.getAudit(),
    3000,
  );

  const [oracleUnderlyingStr, setOracleUnderlyingStr] = useState('0');
  const [oraclePriceStr, setOraclePriceStr] = useState('');
  const [busy, setBusy] = useState(false);
  const [haltTarget, setHaltTarget] = useState<number | null>(null);
  const [resumeTarget, setResumeTarget] = useState<number | null>(null);
  const [status, setStatus] = useState<string | null>(null);

  const underlyings = [...store.underlyings.values()].sort((a, b) => a.underlying - b.underlying);
  const oracleUnderlying = parseUInt(oracleUnderlyingStr, 4);
  const oraclePrice = parsePriceTicks(oraclePriceStr);

  async function run(fn: () => Promise<unknown>, okMsg: string): Promise<boolean> {
    setBusy(true);
    try {
      await fn();
      push('success', okMsg);
      setStatus(`ok — ${okMsg}`);
      return true;
    } catch (e) {
      const d = describeError(e);
      push('error', 'Admin action failed', d);
      setStatus(`error — ${d}`);
      return false;
    } finally {
      setBusy(false);
    }
  }

  async function setOracle(): Promise<void> {
    if (oracleUnderlying === null || oraclePrice === null || busy) return;
    await run(
      () => client.setOracleMark({ underlying: oracleUnderlying, price: oraclePrice }),
      `Oracle mark set — U${oracleUnderlying} @ ${fmtTicks(oraclePrice)}`,
    );
  }

  async function doHalt(): Promise<void> {
    if (haltTarget === null) return;
    const u = haltTarget;
    setHaltTarget(null);
    await run(() => client.haltUnderlying(u), `Underlying U${u} halted`);
  }

  async function doResume(): Promise<void> {
    if (resumeTarget === null) return;
    const u = resumeTarget;
    setResumeTarget(null);
    await run(() => client.resumeUnderlying(u), `Underlying U${u} resumed`);
  }

  return (
    <Panel
      title="Market operations"
      right={
        audit !== null ? (
          <Badge tone={audit.conservation ? 'up' : 'down'}>
            {audit.conservation ? 'conservation ok' : 'conservation violated'}
          </Badge>
        ) : undefined
      }
      className="h-full min-h-0"
      bodyClassName="overflow-y-auto"
    >
      <div className="flex flex-col gap-3 p-2">
        {/* per-underlying live status + halt/resume */}
        <div>
          <p className="font-sans text-[9px] font-semibold tracking-[0.14em] text-txt-dim uppercase">
            underlyings
          </p>
          {underlyings.length === 0 ? (
            <EmptyHint text="No underlyings listed yet" />
          ) : (
            <ul className="mt-1 flex flex-col gap-1">
              {underlyings.map((u) => (
                <li
                  key={u.underlying}
                  className="flex items-center gap-2 rounded-sm border border-edge bg-bg/40 px-2 py-1 font-mono text-[11px] tabular-nums"
                >
                  <span className="w-8 text-txt-dim">U{u.underlying}</span>
                  <span className="min-w-20 text-right">{fmtTicks(u.mark)}</span>
                  {u.halted ? (
                    <Badge tone="down">halted</Badge>
                  ) : (
                    <Badge tone="up">live</Badge>
                  )}
                  {u.fast && <Badge tone="warn">fast</Badge>}
                  <span className="ml-auto flex gap-1">
                    <button
                      type="button"
                      className="btn btn-ghost h-6 px-1.5 text-[9px]"
                      disabled={u.halted || busy || store.offline}
                      onClick={() => setHaltTarget(u.underlying)}
                    >
                      halt
                    </button>
                    <button
                      type="button"
                      className="btn btn-ghost h-6 px-1.5 text-[9px]"
                      disabled={!u.halted || busy || store.offline}
                      onClick={() => setResumeTarget(u.underlying)}
                    >
                      resume
                    </button>
                  </span>
                </li>
              ))}
            </ul>
          )}
        </div>

        <div className="border-t border-edge" />

        {/* oracle mark */}
        <div className="grid grid-cols-[minmax(0,1fr)_minmax(0,1fr)_auto] items-end gap-2">
          <Field
            label="Underlying"
            error={oracleUnderlyingStr !== '' && oracleUnderlying === null ? 'unsigned integer required' : null}
          >
            <input
              className="field"
              value={oracleUnderlyingStr}
              onChange={(e) => setOracleUnderlyingStr(e.target.value)}
              inputMode="numeric"
              placeholder="0"
              disabled={busy}
              aria-label="Oracle underlying"
            />
          </Field>
          <Field
            label="Mark (price)"
            hint={oraclePrice !== null ? `${fmtInt(oraclePrice)} ticks` : undefined}
            error={oraclePriceStr !== '' && oraclePrice === null ? 'price like 5000.00 required' : null}
          >
            <input
              className="field"
              value={oraclePriceStr}
              onChange={(e) => setOraclePriceStr(e.target.value)}
              inputMode="decimal"
              placeholder="5000.00"
              disabled={busy}
              aria-label="Oracle mark price"
            />
          </Field>
          <button
            type="button"
            className="btn btn-accent"
            disabled={oracleUnderlying === null || oraclePrice === null || busy || store.offline}
            onClick={() => void setOracle()}
          >
            set mark
          </button>
        </div>

        {/* audit status */}
        <div>
          <p className="font-sans text-[9px] font-semibold tracking-[0.14em] text-txt-dim uppercase">
            audit ({audit !== null ? `${fmtInt(audit.shards.length)} shards` : '…'})
          </p>
          {auditError !== null ? (
            <p className="mt-1 text-[10px] leading-snug text-warn-bright">
              audit unavailable — {auditError}{' '}
              <button type="button" className="btn btn-ghost h-5 px-1.5 text-[9px]" onClick={refreshAudit}>
                retry
              </button>
            </p>
          ) : audit === null ? (
            <p className="mt-1 text-[10px] text-txt-dim">polling /admin/audit every 3s…</p>
          ) : (
            <ul className="mt-1 flex flex-col gap-0.5 font-mono text-[10px] tabular-nums text-txt-dim">
              {audit.shards.map((s) => (
                <li key={s.shard}>
                  shard {s.shard} · U{s.underlying} · {fmtInt(s.book_orders)} book orders ·{' '}
                  {fmtInt(s.live_accounts)} accounts · state{' '}
                  <span className="text-txt">{s.state_hash.slice(0, 10)}</span>
                </li>
              ))}
              <li>
                gateway · {fmtInt(audit.gateway.conns)} conns · {fmtInt(audit.gateway.ws)} ws ·{' '}
                {fmtInt(audit.gateway.auth_fails)} auth fails
              </li>
            </ul>
          )}
        </div>

        {status && (
          <p className="rounded-sm border border-edge bg-bg/40 px-2 py-1 font-mono text-[10px] leading-snug break-words text-txt-dim">
            {status}
          </p>
        )}
      </div>

      <ConfirmDialog
        open={haltTarget !== null}
        title={`Halt underlying U${haltTarget ?? ''}`}
        confirmLabel="HALT"
        danger={true}
        busy={busy}
        onConfirm={() => void doHalt()}
        onClose={() => setHaltTarget(null)}
      >
        <p>
          Matching and order entry for this underlying stop immediately. Existing orders remain
          resting and resumes are allowed at any time.
        </p>
      </ConfirmDialog>

      <ConfirmDialog
        open={resumeTarget !== null}
        title={`Resume underlying U${resumeTarget ?? ''}`}
        confirmLabel="RESUME"
        busy={busy}
        onConfirm={() => void doResume()}
        onClose={() => setResumeTarget(null)}
      >
        <p>The underlying returns to continuous trading with its current oracle mark.</p>
      </ConfirmDialog>
    </Panel>
  );
}
