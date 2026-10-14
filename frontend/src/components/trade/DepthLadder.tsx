'use client';

/**
 * DepthLadder — top-10 bids/asks for the focused symbol (store depth, fed by
 * the WS `depth` channel with a REST baseline). Qty bars scale to the max
 * displayed size; the spread row shows the tick spread and mid; clicking a
 * price loads it into the order ticket.
 */
import { useTerminal } from '@/providers/TerminalProvider';
import { useStoreSection } from '@/lib/store';
import { fmtTicks } from '@/lib/format';
import type { PriceLevel } from '@/lib/types';
import { Panel } from '@/components/Panel';
import { TableSkeleton } from '@/components/Skeleton';
import { EmptyHint } from '@/components/EmptyHint';
import { symbolLabel } from '@/components/chain/symbolLabel';

export function DepthLadder() {
  const { store, sel, patchTicket } = useTerminal();
  useStoreSection(store, 'depth');

  const sym = sel.symbol !== null ? store.symbols.get(sel.symbol) : undefined;
  const frame = sel.symbol !== null ? store.depth.get(sel.symbol) : undefined;

  const asks = (frame?.asks ?? []).slice(0, 10);
  const bids = (frame?.bids ?? []).slice(0, 10);

  let maxQty = 1;
  for (const l of asks) maxQty = Math.max(maxQty, l[1]);
  for (const l of bids) maxQty = Math.max(maxQty, l[1]);

  const bestBid = bids[0]?.[0];
  const bestAsk = asks[0]?.[0];
  const spread =
    bestBid !== undefined && bestAsk !== undefined && bestAsk >= bestBid
      ? bestAsk - bestBid
      : null;
  const mid =
    bestBid !== undefined && bestAsk !== undefined
      ? Math.floor((bestBid + bestAsk) / 2)
      : null;

  const pick = (price: number): void => {
    patchTicket({ priceStr: fmtTicks(price) });
  };

  return (
    <Panel
      title="Depth"
      right={
        <span className="font-mono text-[10px] text-txt-dim">{symbolLabel(sym)}</span>
      }
      className="h-full min-h-0"
      bodyClassName="flex min-h-0 flex-col"
    >
      {sel.symbol === null ? (
        <EmptyHint text="Select a symbol in the chain" />
      ) : frame === undefined ? (
        <TableSkeleton rows={9} cols={2} label="loading depth" />
      ) : (
        <div className="flex min-h-0 flex-1 flex-col overflow-y-auto" aria-label="Order book">
          {/* asks: worst at top, best adjacent to the spread row */}
          <div className="flex flex-col justify-end">
            {[...asks].reverse().map((l, i) => (
              <LevelRow key={`a${i}-${l[0]}`} level={l} maxQty={maxQty} tone="ask" onPick={pick} />
            ))}
          </div>

          <div className="sticky top-0 z-10 flex shrink-0 items-center justify-between border-y border-edge bg-edge/25 px-2 py-1 font-mono text-[10px] tabular-nums text-txt-dim">
            <span>
              spread{' '}
              {spread !== null ? (
                <span className="text-txt">
                  {spread}t · {fmtTicks(spread)}
                </span>
              ) : (
                '—'
              )}
            </span>
            <span>
              mid {mid !== null ? <span className="text-txt">{fmtTicks(mid)}</span> : '—'}
            </span>
          </div>

          {/* bids: best at top */}
          <div className="flex flex-col">
            {bids.map((l, i) => (
              <LevelRow key={`b${i}-${l[0]}`} level={l} maxQty={maxQty} tone="bid" onPick={pick} />
            ))}
          </div>

          {asks.length === 0 && bids.length === 0 && (
            <EmptyHint text="No depth published for this symbol" />
          )}
        </div>
      )}
    </Panel>
  );
}

function LevelRow({
  level,
  maxQty,
  tone,
  onPick,
}: {
  level: PriceLevel;
  maxQty: number;
  tone: 'ask' | 'bid';
  onPick: (price: number) => void;
}) {
  const pct = Math.min(100, Math.max(3, Math.round((level[1] / maxQty) * 100)));
  return (
    <button
      type="button"
      onClick={() => onPick(level[0])}
      title="Click to load this price into the order ticket"
      className="relative grid h-[22px] grid-cols-[minmax(0,1fr)_56px] items-center px-2 font-mono text-[11px] tabular-nums hover:bg-edge/40"
    >
      <span
        className={`absolute inset-y-0 right-0 ${tone === 'ask' ? 'bg-down/10' : 'bg-up/10'}`}
        style={{ width: `${pct}%` }}
        aria-hidden
      />
      <span className={`relative text-left ${tone === 'ask' ? 'text-down-bright' : 'text-up-bright'}`}>
        {fmtTicks(level[0])}
      </span>
      <span className="relative text-right text-txt-dim">{level[1]}</span>
    </button>
  );
}
