'use client';

/**
 * Tape — recent trades for the focused symbol. Price is colored by the
 * aggressor (buy green / sell red / auction amber); timestamps carry
 * millisecond precision (HH:MM:SS.mmm).
 */
import { useTerminal } from '@/providers/TerminalProvider';
import { useStoreSection } from '@/lib/store';
import { fmtClockMs, fmtTicks } from '@/lib/format';
import type { Aggressor } from '@/lib/types';
import { Panel } from '@/components/Panel';
import { EmptyHint } from '@/components/EmptyHint';
import { symbolLabel } from '@/components/chain/symbolLabel';

const AGGRESSOR_COLOR: Record<Aggressor, string> = {
  buy: 'text-up-bright',
  sell: 'text-down-bright',
  auction: 'text-warn-bright',
};

export function Tape() {
  const { store, sel } = useTerminal();
  useStoreSection(store, 'tape');

  const sym = sel.symbol !== null ? store.symbols.get(sel.symbol) : undefined;
  const trades = sel.symbol !== null ? (store.tape.get(sel.symbol) ?? []) : [];

  return (
    <Panel
      title="Tape"
      right={
        <>
          <span className="font-mono text-[10px] text-txt-dim tabular-nums">{trades.length}</span>
          <span className="hidden font-mono text-[10px] text-txt-dim sm:inline">
            {symbolLabel(sym)}
          </span>
        </>
      }
      className="h-full min-h-0"
      bodyClassName="flex min-h-0 flex-col"
    >
      {sel.symbol === null ? (
        <EmptyHint text="Select a symbol in the chain" />
      ) : trades.length === 0 ? (
        <EmptyHint text="No trades yet for this symbol" />
      ) : (
        <div className="min-h-0 flex-1 overflow-y-auto" aria-label="Time and sales">
          <div className="sticky top-0 z-10 grid grid-cols-[86px_minmax(0,1fr)_52px_56px] bg-panel px-2 py-1 font-sans text-[9px] font-semibold tracking-[0.1em] text-txt-dim uppercase">
            <span>time</span>
            <span>price</span>
            <span className="text-right">qty</span>
            <span className="text-right">side</span>
          </div>
          {trades.map((t, i) => (
            <div
              key={`${t.ms}-${i}`}
              className="grid grid-cols-[86px_minmax(0,1fr)_52px_56px] items-center border-b border-edge/20 px-2 font-mono text-[11px] tabular-nums"
            >
              <span className="text-txt-dim">{fmtClockMs(t.ms)}</span>
              <span className={AGGRESSOR_COLOR[t.aggressor]}>{fmtTicks(t.price)}</span>
              <span className="text-right">{t.qty}</span>
              <span className="text-right text-[9px] tracking-wider text-txt-dim uppercase">
                {t.aggressor}
              </span>
            </div>
          ))}
        </div>
      )}
    </Panel>
  );
}
