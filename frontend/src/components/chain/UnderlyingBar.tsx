'use client';

import { useTerminal } from '@/providers/TerminalProvider';
import { useStoreSection } from '@/lib/store';
import { fmtTicks, fmtTicksSigned } from '@/lib/format';
import { Badge } from '@/components/StatusDot';

/** Underlying ticker strip: mark, day ref, delta, fast-market + halted badges. */
export function UnderlyingBar() {
  const { store, sel, setUnderlying } = useTerminal();
  useStoreSection(store, 'underlying');
  useStoreSection(store, 'chain-rev');

  const underlyings = [...store.underlyings.keys()].sort((a, b) => a - b);
  const u = sel.underlying !== null ? store.underlyings.get(sel.underlying) : undefined;
  const delta = u ? u.mark - u.ref : 0;

  return (
    <div className="flex h-9 shrink-0 items-center gap-3 rounded border border-edge bg-panel px-3 font-mono text-xs tabular-nums">
      {underlyings.length > 1 ? (
        <label className="flex items-center gap-1 font-sans text-[10px] font-semibold tracking-wider text-txt-dim uppercase">
          <span className="sr-only sm:not-sr-only">und</span>
          <select
            className="field h-6 w-16 py-0"
            value={sel.underlying ?? ''}
            onChange={(e) => setUnderlying(Number(e.target.value))}
            aria-label="Select underlying"
          >
            {underlyings.map((id) => (
              <option key={id} value={id}>
                U{id}
              </option>
            ))}
          </select>
        </label>
      ) : (
        <span className="font-sans text-[10px] font-semibold tracking-[0.12em] text-txt-dim uppercase">
          U{sel.underlying ?? '—'}
        </span>
      )}
      {u ? (
        <>
          <span className="text-sm">{fmtTicks(u.mark)}</span>
          <span className={delta >= 0 ? 'text-up-bright' : 'text-down-bright'}>
            {fmtTicksSigned(delta)}
          </span>
          <span className="text-[10px] text-txt-dim" title="day reference price">
            ref {fmtTicks(u.ref)}
          </span>
          <span className="ml-auto flex items-center gap-1.5">
            {u.fast && <Badge tone="warn" title="fast-market regime — wider bands">fast</Badge>}
            {u.halted && (
              <Badge tone="down" title="underlying halted">
                halted{u.haltLevel !== undefined ? ` L${u.haltLevel}` : ''}
              </Badge>
            )}
          </span>
        </>
      ) : (
        <span className="text-[11px] text-txt-dim">no underlying data</span>
      )}
    </div>
  );
}
