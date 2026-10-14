'use client';

/**
 * OpenOrders — the live working-order list (store.orders, kept current by the
 * REST snapshot plus ack/fill/canceled WS events). Per-order cancel and
 * cancel-all via VenueClient.cancelOrder; side is colored, leaves shown.
 */
import { useState } from 'react';
import { useTerminal } from '@/providers/TerminalProvider';
import { useStoreSection } from '@/lib/store';
import { fmtClock, fmtTicks } from '@/lib/format';
import { Panel } from '@/components/Panel';
import { EmptyHint } from '@/components/EmptyHint';
import { symbolLabel } from '@/components/chain/symbolLabel';

export function OpenOrders() {
  const { store, cancelOrder, cancelAll } = useTerminal();
  useStoreSection(store, 'orders');

  const [busy, setBusy] = useState(false);
  const rows = [...store.orders.values()].sort((a, b) => b.ms - a.ms);

  async function onCancelAll(): Promise<void> {
    if (busy) return;
    setBusy(true);
    try {
      await cancelAll();
    } finally {
      setBusy(false);
    }
  }

  return (
    <Panel
      title="Open orders"
      right={
        <>
          <span className="font-mono text-[10px] text-txt-dim tabular-nums">{rows.length}</span>
          <button
            type="button"
            className="btn btn-ghost h-5 px-1.5 text-[9px]"
            disabled={rows.length === 0 || busy}
            onClick={() => void onCancelAll()}
          >
            cancel all
          </button>
        </>
      }
      className="h-full min-h-0"
      bodyClassName="overflow-y-auto"
    >
      {rows.length === 0 ? (
        <EmptyHint text="No working orders" />
      ) : (
        <table className="w-full font-mono text-[11px] tabular-nums">
          <thead>
            <tr className="sticky top-0 z-10 bg-panel font-sans text-[9px] font-semibold tracking-[0.1em] text-txt-dim uppercase">
              <th className="px-1 py-1 text-left font-semibold">id</th>
              <th className="px-1 py-1 text-left font-semibold">symbol</th>
              <th className="px-1 py-1 text-center font-semibold">side</th>
              <th className="px-1 py-1 text-right font-semibold">price</th>
              <th className="px-1 py-1 text-right font-semibold">leaves</th>
              <th className="px-1 py-1 text-center font-semibold">type</th>
              <th className="px-1 py-1 text-left font-semibold">time</th>
              <th className="px-1 py-1" aria-label="Cancel" />
            </tr>
          </thead>
          <tbody>
            {rows.map((o) => (
              <tr key={o.order} className="border-b border-edge/30 hover:bg-edge/20">
                <td className="px-1 py-1 whitespace-nowrap text-txt-dim">#{o.order}</td>
                <td className="px-1 py-1 whitespace-nowrap">
                  {symbolLabel(store.symbols.get(o.symbol))}
                </td>
                <td
                  className={`px-1 py-1 text-center font-bold ${
                    o.partial ? 'text-txt-dim' : o.side === 'buy' ? 'text-up-bright' : 'text-down-bright'
                  }`}
                >
                  {o.partial ? '—' : o.side}
                </td>
                <td className="px-1 py-1 text-right whitespace-nowrap">
                  {o.partial ? '—' : fmtTicks(o.price)}
                </td>
                <td className="px-1 py-1 text-right whitespace-nowrap">
                  {o.leaves}
                  <span className="text-txt-dim">/{o.qty}</span>
                </td>
                <td className="px-1 py-1 text-center text-[9px] tracking-wider whitespace-nowrap text-txt-dim uppercase">
                  {o.kind === 'market' ? 'mkt' : o.tif}
                  {o.post_only ? ' po' : ''}
                </td>
                <td className="px-1 py-1 whitespace-nowrap text-txt-dim">{fmtClock(o.ms)}</td>
                <td className="px-1 py-1 text-right">
                  <button
                    type="button"
                    onClick={() => void cancelOrder(o.order, o.symbol)}
                    aria-label={`Cancel order ${o.order}`}
                    title={`Cancel order ${o.order}`}
                    className="rounded-sm px-1.5 text-xs font-bold text-down-bright hover:bg-down/20"
                  >
                    ×
                  </button>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </Panel>
  );
}
