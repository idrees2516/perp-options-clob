'use client';

/**
 * SessionsCard — lists a new 0DTE session (contract §2.4 POST /admin/sessions):
 * underlying + expiry datetime, auto-strike ladder generated ±3% around the
 * live mark in 500-tick increments (generateStrikes, integer math), call/put
 * kinds. The anchor pre-fills from the live underlying mark and can be
 * overridden (fresh engines without any mark yet).
 */
import { useEffect, useState } from 'react';
import { useTerminal } from '@/providers/TerminalProvider';
import { useStoreSection } from '@/lib/store';
import { useToast } from '@/components/Toast';
import {
  fmtHM,
  fmtInt,
  fmtTicks,
  generateStrikes,
  parsePriceTicks,
  parseUInt,
  toLocalInputValue,
} from '@/lib/format';
import { describeError } from '@/lib/messages';
import type { OptionKind } from '@/lib/types';
import { Panel } from '@/components/Panel';
import { Field } from '@/components/Field';

/** Next 16:00 local (or tomorrow's when past) — the classic 0DTE expiry. */
function defaultExpiryMs(now: number): number {
  const d = new Date(now);
  d.setHours(16, 0, 0, 0);
  if (d.getTime() <= now) d.setDate(d.getDate() + 1);
  return d.getTime();
}

export function SessionsCard() {
  const { client, store, refreshSymbols } = useTerminal();
  const { push } = useToast();
  useStoreSection(store, 'underlying');
  useStoreSection(store, 'conn');
  useStoreSection(store, 'clock');

  const [underlyingStr, setUnderlyingStr] = useState('0');
  const [expiryInput, setExpiryInput] = useState(() => toLocalInputValue(defaultExpiryMs(Date.now())));
  const [callOn, setCallOn] = useState(true);
  const [putOn, setPutOn] = useState(true);
  const [anchorStr, setAnchorStr] = useState('');
  const [anchorTouched, setAnchorTouched] = useState(false);
  const [busy, setBusy] = useState(false);
  const [created, setCreated] = useState<number[] | null>(null);
  const [error, setError] = useState<string | null>(null);

  const underlying = parseUInt(underlyingStr, 4);
  const liveMark = underlying !== null ? store.underlyings.get(underlying)?.mark : undefined;

  // pre-fill the anchor from the live mark until the operator edits it
  useEffect(() => {
    if (!anchorTouched && liveMark !== undefined && liveMark > 0) setAnchorStr(fmtTicks(liveMark));
  }, [liveMark, anchorTouched]);

  const anchor = parsePriceTicks(anchorStr);
  const strikes = anchor !== null ? generateStrikes(anchor, 3, 500) : [];
  const expiryMs = Date.parse(expiryInput);
  const expiryValid = Number.isFinite(expiryMs) && expiryMs > store.serverNow();
  const kinds: OptionKind[] = [];
  if (callOn) kinds.push('call');
  if (putOn) kinds.push('put');

  const cantCreateReason =
    underlying === null
      ? 'underlying must be an unsigned integer'
      : !expiryValid
        ? 'expiry must be in the future'
        : anchor === null || strikes.length === 0
          ? 'anchor price required to build the ladder'
          : kinds.length === 0
            ? 'select at least one kind'
            : null;

  async function create(): Promise<void> {
    if (cantCreateReason !== null || underlying === null || !expiryValid || busy) return;
    setBusy(true);
    setError(null);
    setCreated(null);
    try {
      const res = await client.createSession({
        underlying,
        expiry_ms: expiryMs,
        strikes,
        kinds,
      });
      setCreated(res.symbols);
      push(
        'success',
        `Session listed — ${res.symbols.length} symbols`,
        `U${underlying} · expires ${fmtHM(expiryMs)} · ${strikes.length} strikes × ${kinds.length} kinds`,
      );
      refreshSymbols();
    } catch (e) {
      const d = describeError(e);
      setError(d);
      push('error', 'Session create failed', d);
    } finally {
      setBusy(false);
    }
  }

  return (
    <Panel title="Session ladder" className="h-full min-h-0" bodyClassName="overflow-y-auto">
      <div className="flex flex-col gap-2 p-2">
        <div className="grid grid-cols-2 gap-2">
          <Field
            label="Underlying"
            error={underlyingStr !== '' && underlying === null ? 'unsigned integer required' : null}
            hint={liveMark !== undefined ? `live mark ${fmtTicks(liveMark)}` : 'no live mark yet'}
          >
            <input
              className="field"
              value={underlyingStr}
              onChange={(e) => setUnderlyingStr(e.target.value)}
              inputMode="numeric"
              placeholder="0"
              disabled={busy}
              aria-label="Underlying id"
            />
          </Field>
          <Field
            label="Expiry (local)"
            error={!expiryValid ? 'must be a future datetime' : null}
          >
            <input
              className="field"
              type="datetime-local"
              value={expiryInput}
              onChange={(e) => setExpiryInput(e.target.value)}
              disabled={busy}
              aria-label="Session expiry"
            />
          </Field>
        </div>

        <Field
          label="Anchor / mark (ticks)"
          hint={
            anchor !== null
              ? `±3% ladder · ${fmtInt(strikes.length)} strikes · step 500t`
              : 'auto-filled from the live mark — editable'
          }
          error={anchorStr !== '' && anchor === null ? 'price like 5000.00 required' : null}
        >
          <input
            className="field"
            value={anchorStr}
            onChange={(e) => {
              setAnchorTouched(true);
              setAnchorStr(e.target.value);
            }}
            inputMode="decimal"
            placeholder="5000.00"
            disabled={busy}
            aria-label="Ladder anchor price"
          />
        </Field>

        <div className="flex items-center gap-4">
          <label className="flex items-center gap-1.5 font-sans text-[10px] font-semibold tracking-wider text-txt-dim uppercase">
            <input
              type="checkbox"
              className="h-3.5 w-3.5 accent-accent-bright"
              checked={callOn}
              disabled={busy}
              onChange={(e) => setCallOn(e.target.checked)}
            />
            calls
          </label>
          <label className="flex items-center gap-1.5 font-sans text-[10px] font-semibold tracking-wider text-txt-dim uppercase">
            <input
              type="checkbox"
              className="h-3.5 w-3.5 accent-accent-bright"
              checked={putOn}
              disabled={busy}
              onChange={(e) => setPutOn(e.target.checked)}
            />
            puts
          </label>
        </div>

        {/* strike preview */}
        {strikes.length > 0 && (
          <div className="rounded-sm border border-edge bg-bg/40 p-2">
            <p className="font-sans text-[9px] font-semibold tracking-[0.14em] text-txt-dim uppercase">
              generated strikes ({fmtInt(strikes.length)})
            </p>
            <div className="mt-1 max-h-24 overflow-y-auto font-mono text-[10px] leading-relaxed tabular-nums text-txt-dim">
              {strikes.map((s) => (
                <span key={s} className="mr-3 inline-block">
                  <span className="text-txt">{fmtTicks(s)}</span> ({fmtInt(s)}t)
                </span>
              ))}
            </div>
          </div>
        )}

        <button
          type="button"
          className="btn btn-accent"
          disabled={cantCreateReason !== null || busy || store.offline}
          title={cantCreateReason ?? undefined}
          onClick={() => void create()}
        >
          {busy ? 'listing' : 'create session'}
        </button>

        {error && (
          <p
            role="alert"
            className="rounded-sm border border-down/40 bg-down/10 px-2 py-1.5 text-[11px] leading-snug text-down-bright"
          >
            {error}
          </p>
        )}

        {created !== null && (
          <div className="rounded-sm border border-up/40 bg-up/10 p-2">
            <p className="font-sans text-[9px] font-semibold tracking-[0.14em] text-up-bright uppercase">
              listed — {fmtInt(created.length)} symbols
            </p>
            <p className="mt-1 font-mono text-[10px] leading-relaxed break-words text-txt-dim tabular-nums">
              {created.join(', ')}
            </p>
          </div>
        )}
      </div>
    </Panel>
  );
}
