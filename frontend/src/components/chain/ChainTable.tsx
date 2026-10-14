'use client';

/**
 * ChainTable — the option chain for the selected underlying.
 *
 * Expiries are grouped into tabs (local HH:MM + live countdown + phase badge
 * CONTINUOUS/AUCTION/EXPIRED; countdown turns red under 10 minutes). The
 * strike ladder pairs calls (left) and puts (right) per strike; clicking a
 * quote cell selects that symbol in the terminal (TerminalProvider). The
 * selected row/column is highlighted and the ATM strike is accented.
 */
import { useMemo } from 'react';
import { useTerminal } from '@/providers/TerminalProvider';
import { useServerClock, useStoreSection, type SymbolLive } from '@/lib/store';
import type { Phase } from '@/lib/types';
import { fmtCountdown, fmtHM, fmtInt, fmtTicks } from '@/lib/format';
import { Badge } from '@/components/StatusDot';
import { Panel } from '@/components/Panel';
import { TableSkeleton } from '@/components/Skeleton';
import { EmptyHint } from '@/components/EmptyHint';

interface StrikeRow {
  strike: number;
  call?: SymbolLive;
  put?: SymbolLive;
}

interface ExpiryGroup {
  expiryMs: number;
  phase: Phase;
  rows: StrikeRow[];
}

const PHASE_TONE: Record<Phase, 'up' | 'warn' | 'neutral'> = {
  continuous: 'up',
  auction: 'warn',
  expired: 'neutral',
};

function phaseOf(syms: SymbolLive[]): Phase {
  const phases = new Set(syms.map((s) => s.phase));
  if (phases.has('auction')) return 'auction';
  if (phases.size === 1 && phases.has('expired')) return 'expired';
  return 'continuous';
}

export function ChainTable() {
  const { store, sel, ready, setExpiry, selectSymbol } = useTerminal();
  useStoreSection(store, 'chain-rev'); // symbol list membership
  useStoreSection(store, 'chain'); // live bbo/mark/phase
  const now = useServerClock(store, 1000);

  const groups = useMemo<ExpiryGroup[]>(() => {
    if (sel.underlying === null) return [];
    const byExpiry = new Map<number, SymbolLive[]>();
    for (const s of store.symbols.values()) {
      if (s.underlying !== sel.underlying) continue;
      const arr = byExpiry.get(s.expiry_ms);
      if (arr) arr.push(s);
      else byExpiry.set(s.expiry_ms, [s]);
    }
    const out: ExpiryGroup[] = [];
    for (const [expiryMs, syms] of byExpiry) {
      const byStrike = new Map<number, StrikeRow>();
      for (const s of syms) {
        const row = byStrike.get(s.strike) ?? { strike: s.strike };
        if (s.kind === 'call') row.call = s;
        else row.put = s;
        byStrike.set(s.strike, row);
      }
      out.push({
        expiryMs,
        phase: phaseOf(syms),
        rows: [...byStrike.values()].sort((a, b) => a.strike - b.strike),
      });
    }
    // future expiries first (soonest wins), then the most recently expired
    out.sort((a, b) => {
      const fa = a.expiryMs >= now;
      const fb = b.expiryMs >= now;
      if (fa && fb) return a.expiryMs - b.expiryMs;
      if (!fa && !fb) return b.expiryMs - a.expiryMs;
      return fa ? -1 : 1;
    });
    return out;
  }, [store, sel.underlying, now]);

  const active = groups.find((g) => g.expiryMs === sel.expiryMs) ?? groups[0];
  const mark = sel.underlying !== null ? store.underlyings.get(sel.underlying)?.mark : undefined;

  const atmStrike = useMemo(() => {
    if (active === undefined || mark === undefined) return null;
    let best: number | null = null;
    for (const r of active.rows) {
      if (best === null || Math.abs(r.strike - mark) < Math.abs(best - mark)) best = r.strike;
    }
    return best;
  }, [active, mark]);

  return (
    <Panel
      title={`Chain · U${sel.underlying ?? '—'}`}
      right={
        <span className="font-mono text-[10px] text-txt-dim tabular-nums">
          {active ? `${active.rows.length} strikes` : ''}
        </span>
      }
      className="h-full min-h-0"
      bodyClassName="flex min-h-0 flex-col"
    >
      {/* expiry tabs */}
      <div
        className="flex shrink-0 items-center gap-1 overflow-x-auto border-b border-edge p-1"
        role="tablist"
        aria-label="Expiry sessions"
      >
        {groups.map((g) => {
          const cd = fmtCountdown(g.expiryMs - now);
          const isActive = active !== undefined && active.expiryMs === g.expiryMs;
          return (
            <button
              key={g.expiryMs}
              type="button"
              role="tab"
              aria-selected={isActive}
              onClick={() => setExpiry(g.expiryMs)}
              className={`flex shrink-0 items-center gap-1.5 rounded-sm border px-1.5 py-0.5 ${
                isActive ? 'border-accent/50 bg-accent/10' : 'border-edge hover:bg-edge/40'
              }`}
            >
              <span className="font-mono text-[11px] font-semibold tabular-nums">
                {fmtHM(g.expiryMs)}
              </span>
              <span
                className={`font-mono text-[10px] tabular-nums ${cd.urgent ? 'text-down-bright' : 'text-txt-dim'}`}
              >
                {cd.text}
              </span>
              <Badge tone={PHASE_TONE[g.phase]}>{g.phase}</Badge>
            </button>
          );
        })}
        {groups.length === 0 && <span className="px-1 text-[10px] text-txt-dim">no expiries</span>}
      </div>

      {/* column header — calls left, strike center, puts right (mirrored) */}
      <div className="sticky top-0 z-10 grid shrink-0 grid-cols-[minmax(0,1fr)_72px_minmax(0,1fr)] border-b border-edge bg-panel">
        <div className="grid grid-cols-4 px-1 py-1 font-sans text-[9px] font-semibold tracking-[0.1em] text-txt-dim uppercase">
          <span className="col-span-4 pb-0.5 text-center text-[9px] text-txt-dim/70">calls</span>
          <span>bid</span>
          <span>ask</span>
          <span>mark</span>
          <span className="text-right">vol</span>
        </div>
        <div className="flex items-center justify-center py-1 font-sans text-[9px] font-semibold tracking-[0.1em] text-txt-dim uppercase">
          strike
        </div>
        <div className="grid grid-cols-4 px-1 py-1 font-sans text-[9px] font-semibold tracking-[0.1em] text-txt-dim uppercase">
          <span className="col-span-4 pb-0.5 text-center text-[9px] text-txt-dim/70">puts</span>
          <span>vol</span>
          <span>mark</span>
          <span>ask</span>
          <span className="text-right">bid</span>
        </div>
      </div>

      {/* strike ladder */}
      <div className="min-h-0 flex-1 overflow-y-auto">
        {active === undefined ? (
          !ready && store.symbols.size === 0 ? (
            <TableSkeleton rows={10} cols={8} label="loading chain" />
          ) : (
            <EmptyHint text="No listed sessions for this underlying" />
          )
        ) : (
          active.rows.map((r) => {
            const selectedCall = r.call !== undefined && r.call.symbol === sel.symbol;
            const selectedPut = r.put !== undefined && r.put.symbol === sel.symbol;
            return (
              <div
                key={r.strike}
                className={`grid grid-cols-[minmax(0,1fr)_72px_minmax(0,1fr)] items-stretch border-b border-edge/30 ${
                  selectedCall || selectedPut ? 'bg-edge/30' : ''
                }`}
              >
                <QuoteCells
                  sym={r.call}
                  selected={selectedCall}
                  mirrored={false}
                  onPick={selectSymbol}
                />
                <div
                  className={`flex items-center justify-center font-mono text-[11px] font-semibold tabular-nums ${
                    r.strike === atmStrike ? 'bg-accent/10 text-accent-bright' : 'text-txt'
                  }`}
                  title={r.strike === atmStrike ? 'at-the-money strike' : undefined}
                >
                  {fmtTicks(r.strike)}
                </div>
                <QuoteCells sym={r.put} selected={selectedPut} mirrored={true} onPick={selectSymbol} />
              </div>
            );
          })
        )}
      </div>
    </Panel>
  );
}

/** One side of a strike row: 4 quote cells, clickable to select the symbol. */
function QuoteCells({
  sym,
  selected,
  mirrored,
  onPick,
}: {
  sym?: SymbolLive;
  selected: boolean;
  mirrored: boolean;
  onPick: (symbol: number) => void;
}) {
  if (sym === undefined) {
    return (
      <div className="grid grid-cols-4 items-center px-1 font-mono text-[11px] text-txt-dim/30">
        <span>·</span>
        <span>·</span>
        <span>·</span>
        <span className="text-right">·</span>
      </div>
    );
  }
  const cells = [
    { v: sym.bid > 0 ? fmtTicks(sym.bid) : '—', cls: 'text-up-bright' },
    { v: sym.ask > 0 ? fmtTicks(sym.ask) : '—', cls: 'text-down-bright' },
    { v: fmtTicks(sym.mark), cls: 'text-txt' },
    { v: fmtInt(sym.vol), cls: 'text-txt-dim' },
  ];
  const ordered = mirrored ? [...cells].reverse() : cells;
  const label = `${sym.kind} ${fmtTicks(sym.strike)} ${fmtHM(sym.expiry_ms)}`;
  return (
    <button
      type="button"
      onClick={() => onPick(sym.symbol)}
      aria-pressed={selected}
      aria-label={`Select ${label}`}
      title={label}
      className={`grid grid-cols-4 items-center px-1 text-left font-mono text-[11px] tabular-nums ${
        selected ? 'bg-accent/15 ring-1 ring-accent/50 ring-inset' : 'hover:bg-edge/40'
      }`}
    >
      {ordered.map((c, i) => (
        <span
          key={i}
          className={`${c.cls} truncate ${i === 3 ? 'text-right' : ''}`}
        >
          {c.v}
        </span>
      ))}
    </button>
  );
}
