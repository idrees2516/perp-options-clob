'use client';

/**
 * PositionsPanel — signed positions with live client-side intrinsic
 * (underlying mark vs strike, integer math via mulInt) plus the per-underlying
 * balance strip (cash / hold / margin_required / available) from the store.
 */
import { useTerminal } from '@/providers/TerminalProvider';
import { useStoreSection } from '@/lib/store';
import { fmtMoney, fmtQty, fmtTicks, fmtTicksSigned, mulInt } from '@/lib/format';
import { Panel } from '@/components/Panel';
import { EmptyHint } from '@/components/EmptyHint';
import { symbolLabel } from '@/components/chain/symbolLabel';

export function PositionsPanel() {
  const { store } = useTerminal();
  useStoreSection(store, 'positions');
  useStoreSection(store, 'balances');
  useStoreSection(store, 'underlying'); // marks move the live intrinsic

  const rows = [...store.positions.values()]
    .map((p) => {
      const sym = store.symbols.get(p.symbol);
      const und = sym !== undefined ? store.underlyings.get(sym.underlying) : undefined;
      let perCt: number | null = null;
      if (sym !== undefined && und !== undefined) {
        perCt =
          sym.kind === 'call'
            ? Math.max(0, und.mark - sym.strike)
            : Math.max(0, sym.strike - und.mark);
      }
      const absValue = perCt !== null ? mulInt(perCt, Math.abs(p.qty)) : null;
      const value = absValue !== null ? (p.qty >= 0 ? absValue : -absValue) : null;
      return { p, sym, und, perCt, value };
    })
    .sort((a, b) => a.p.symbol - b.p.symbol);

  const balances = [...store.balances.values()].sort((a, b) => a.underlying - b.underlying);

  return (
    <Panel
      title="Positions"
      right={<span className="font-mono text-[10px] text-txt-dim tabular-nums">{rows.length}</span>}
      className="h-full min-h-0"
      bodyClassName="flex min-h-0 flex-col"
    >
      <div className="min-h-0 flex-1 overflow-auto">
        {rows.length === 0 ? (
          <EmptyHint text="Flat — no open positions" />
        ) : (
          <table className="w-full font-mono text-[11px] tabular-nums">
            <thead>
              <tr className="sticky top-0 z-10 bg-panel font-sans text-[9px] font-semibold tracking-[0.1em] text-txt-dim uppercase">
                <th className="px-2 py-1 text-left font-semibold">symbol</th>
                <th className="px-2 py-1 text-right font-semibold">qty</th>
                <th className="px-2 py-1 text-right font-semibold">und mark</th>
                <th className="px-2 py-1 text-right font-semibold">intr/ct</th>
                <th className="px-2 py-1 text-right font-semibold">value</th>
                <th className="px-2 py-1 text-right font-semibold">margin</th>
                <th className="px-2 py-1 text-right font-semibold">delta notl</th>
              </tr>
            </thead>
            <tbody>
              {rows.map(({ p, sym, und, perCt, value }) => (
                <tr key={p.symbol} className="border-b border-edge/30 hover:bg-edge/20">
                  <td className="px-2 py-1 whitespace-nowrap">{symbolLabel(sym)}</td>
                  <td
                    className={`px-2 py-1 text-right font-bold whitespace-nowrap ${
                      p.qty > 0 ? 'text-up-bright' : p.qty < 0 ? 'text-down-bright' : 'text-txt-dim'
                    }`}
                  >
                    {fmtQty(p.qty)}
                  </td>
                  <td className="px-2 py-1 text-right whitespace-nowrap">
                    {und !== undefined ? fmtTicks(und.mark) : '—'}
                  </td>
                  <td className="px-2 py-1 text-right whitespace-nowrap">
                    {perCt !== null ? fmtTicks(perCt) : '—'}
                  </td>
                  <td
                    className={`px-2 py-1 text-right whitespace-nowrap ${
                      value !== null && value > 0
                        ? 'text-up-bright'
                        : value !== null && value < 0
                          ? 'text-down-bright'
                          : 'text-txt'
                    }`}
                  >
                    {value !== null ? fmtTicksSigned(value) : '—'}
                  </td>
                  <td className="px-2 py-1 text-right whitespace-nowrap">{fmtMoney(p.margin)}</td>
                  <td className="px-2 py-1 text-right whitespace-nowrap text-txt-dim">
                    {fmtTicksSigned(p.delta_notional)}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>

      {/* balance strip per underlying */}
      {balances.length > 0 && (
        <div
          className="flex shrink-0 flex-wrap gap-2 border-t border-edge p-2"
          aria-label="Balances per underlying"
        >
          {balances.map((b) => (
            <div
              key={b.underlying}
              className="flex items-center gap-3 rounded-sm border border-edge bg-bg/40 px-2 py-1 font-mono text-[10px] tabular-nums"
            >
              <span className="font-sans text-[9px] font-bold tracking-[0.14em] text-txt-dim uppercase">
                U{b.underlying}
              </span>
              <span>
                cash <span className="text-txt">{fmtMoney(b.cash)}</span>
              </span>
              <span className="text-txt-dim">
                hold <span className="text-txt">{fmtMoney(b.hold)}</span>
              </span>
              <span className="text-txt-dim">
                mreq <span className="text-txt">{fmtMoney(b.margin_required)}</span>
              </span>
              <span className="text-txt-dim">
                avail <span className="text-accent-bright">{fmtMoney(b.available)}</span>
              </span>
            </div>
          ))}
        </div>
      )}
    </Panel>
  );
}
