'use client';

/**
 * OrderTicket — order entry for the selected symbol.
 *
 * Validation (contract §2.3): qty integer 1..=10000, price integer ticks >= 0,
 * market orders carry a mandatory protection price and are forced to IOC.
 * Estimated premium = price × qty in exact integer ticks (mulInt / BigInt).
 * Submit goes through VenueClient.placeOrder; the engine ack (order id +
 * fills) or the reject reason (messages.ts) is surfaced inline. Disabled
 * while the engine is unreachable.
 */
import { useEffect, useState } from 'react';
import { useTerminal } from '@/providers/TerminalProvider';
import { useStoreSection } from '@/lib/store';
import { fmtInt, fmtMoney, fmtTicks, mulInt, parsePriceTicks, parseQty } from '@/lib/format';
import { REJECT_HUMAN, describeError } from '@/lib/messages';
import type { OrderFill, OrderKind, OrderRequest, RejectReason, Side, Tif } from '@/lib/types';
import { Panel } from '@/components/Panel';
import { Field } from '@/components/Field';
import { symbolLabel } from '@/components/chain/symbolLabel';

type TicketResult =
  | { t: 'ok'; orderId: number; leaves: number; fills: OrderFill[] }
  | { t: 'reject'; code: number; reason: RejectReason }
  | { t: 'error'; msg: string };

export function OrderTicket() {
  const { store, client, sel, ticket, patchTicket, refreshAccount, notifyResultSurfaced } =
    useTerminal();
  useStoreSection(store, 'conn');
  useStoreSection(store, 'underlying');

  const [busy, setBusy] = useState(false);
  const [result, setResult] = useState<TicketResult | null>(null);

  // a stale result from a previously selected symbol is misleading — clear it
  useEffect(() => {
    setResult(null);
  }, [sel.symbol]);

  const sym = sel.symbol !== null ? store.symbols.get(sel.symbol) : undefined;
  const und = sym !== undefined ? store.underlyings.get(sym.underlying) : undefined;

  const qty = parseQty(ticket.qtyStr);
  const qtyValid = qty !== null && qty >= 1 && qty <= 10000;
  const price = parsePriceTicks(ticket.priceStr);
  const priceValid = price !== null && price >= 0;
  const tifOk = ticket.kind !== 'market' || ticket.tif === 'ioc';

  const premium = qtyValid && priceValid ? mulInt(price!, qty!) : null;

  const qtyError =
    ticket.qtyStr.trim() === '' || qtyValid
      ? null
      : 'integer 1..=10000';
  const priceError =
    ticket.priceStr.trim() === '' || priceValid
      ? null
      : 'integer ticks >= 0 (max 2 decimals)';

  const noSubmitReason = !tifOk
    ? 'market orders require IOC'
    : sel.symbol === null
      ? 'select a symbol in the chain'
      : !qtyValid
        ? 'qty must be an integer 1..=10000'
        : !priceValid
          ? 'price must be an integer >= 0'
          : store.offline
            ? 'engine unreachable — order entry disabled'
            : null;
  const canSubmit = noSubmitReason === null && !busy;

  function setKind(kind: OrderKind): void {
    patchTicket(kind === 'market' ? { kind, tif: 'ioc', postOnly: false } : { kind });
  }

  async function submit(): Promise<void> {
    if (!canSubmit || sel.symbol === null || qty === null || price === null) return;
    setBusy(true);
    setResult(null);
    const tif: Tif = ticket.kind === 'market' ? 'ioc' : ticket.tif;
    const req: OrderRequest = {
      symbol: sel.symbol,
      side: ticket.side,
      qty,
      price,
      tif,
      kind: ticket.kind,
      post_only: ticket.kind === 'market' ? false : ticket.postOnly,
      reduce_only: ticket.reduceOnly,
    };
    try {
      const res = await client.placeOrder(req);
      notifyResultSurfaced(); // suppress the WS echo toast for this result
      if (res.ok) {
        store.attachLocalOrder(res.order_id, {
          order: res.order_id,
          symbol: req.symbol,
          side: req.side,
          price: req.price,
          qty: req.qty,
          leaves: res.leaves,
          tif: req.tif,
          kind: req.kind,
          post_only: req.post_only,
          ms: Date.now(),
        });
        refreshAccount(); // balances/positions resettle server-side
        setResult({ t: 'ok', orderId: res.order_id, leaves: res.leaves, fills: res.fills });
      } else {
        setResult({ t: 'reject', code: res.code, reason: res.reason });
      }
    } catch (e) {
      setResult({ t: 'error', msg: describeError(e) });
    } finally {
      setBusy(false);
    }
  }

  return (
    <Panel
      title="Order ticket"
      right={<span className="font-mono text-[10px] text-txt-dim">{symbolLabel(sym)}</span>}
      className="h-full min-h-0"
      bodyClassName="overflow-y-auto"
    >
      <div className="flex flex-col gap-2 p-2">
        {sym === undefined && (
          <p className="rounded-sm border border-warn/40 bg-warn/10 px-2 py-1.5 text-[11px] text-warn-bright">
            Select a symbol in the chain to start an order.
          </p>
        )}
        {sym !== undefined && und !== undefined && und.halted && (
          <p className="rounded-sm border border-down/40 bg-down/10 px-2 py-1.5 text-[11px] text-down-bright">
            Underlying U{sym.underlying} is halted — orders will be rejected.
          </p>
        )}
        {sym !== undefined && sym.phase !== 'continuous' && (
          <p className="rounded-sm border border-warn/40 bg-warn/10 px-2 py-1.5 text-[11px] text-warn-bright">
            Symbol is in {sym.phase.toUpperCase()} phase — orders are only accepted in CONTINUOUS.
          </p>
        )}

        {/* side */}
        <div className="grid grid-cols-2 gap-1" role="group" aria-label="Side">
          <SideButton side="buy" active={ticket.side === 'buy'} onClick={() => patchTicket({ side: 'buy' })} />
          <SideButton side="sell" active={ticket.side === 'sell'} onClick={() => patchTicket({ side: 'sell' })} />
        </div>

        {/* qty / price */}
        <div className="grid grid-cols-2 gap-2">
          <Field label="Qty" error={qtyError}>
            <input
              className="field"
              value={ticket.qtyStr}
              onChange={(e) => patchTicket({ qtyStr: e.target.value })}
              inputMode="numeric"
              placeholder="1"
              autoComplete="off"
              disabled={busy}
              aria-label="Quantity (contracts)"
            />
          </Field>
          <Field
            label={ticket.kind === 'market' ? 'Protection price' : 'Price'}
            error={priceError}
            hint={ticket.kind === 'market' ? 'worst acceptable price' : undefined}
          >
            <input
              className="field"
              value={ticket.priceStr}
              onChange={(e) => patchTicket({ priceStr: e.target.value })}
              inputMode="decimal"
              placeholder="2.50"
              autoComplete="off"
              disabled={busy}
              aria-label={
                ticket.kind === 'market' ? 'Protection price (ticks)' : 'Limit price (ticks)'
              }
            />
          </Field>
        </div>

        {/* kind */}
        <div className="grid grid-cols-2 gap-1" role="group" aria-label="Order kind">
          <KindButton kind="limit" active={ticket.kind === 'limit'} onClick={() => setKind('limit')} />
          <KindButton kind="market" active={ticket.kind === 'market'} onClick={() => setKind('market')} />
        </div>

        {/* tif + flags */}
        <Field
          label="Time in force"
          hint={ticket.kind === 'market' ? 'locked to IOC for market orders' : undefined}
        >
          <select
            className="field"
            value={ticket.tif}
            disabled={ticket.kind === 'market' || busy}
            onChange={(e) => patchTicket({ tif: e.target.value as Tif })}
            aria-label="Time in force"
          >
            <option value="gtc">GTC — good till canceled</option>
            <option value="ioc">IOC — immediate or cancel</option>
            <option value="fok">FOK — fill or kill</option>
          </select>
        </Field>

        <div className="grid grid-cols-2 gap-2">
          <label className="flex items-center gap-1.5 font-sans text-[10px] font-semibold tracking-wider text-txt-dim uppercase">
            <input
              type="checkbox"
              className="h-3.5 w-3.5 accent-accent-bright"
              checked={ticket.postOnly}
              disabled={ticket.kind === 'market' || busy}
              onChange={(e) => patchTicket({ postOnly: e.target.checked })}
            />
            post-only
          </label>
          <label className="flex items-center gap-1.5 font-sans text-[10px] font-semibold tracking-wider text-txt-dim uppercase">
            <input
              type="checkbox"
              className="h-3.5 w-3.5 accent-accent-bright"
              checked={ticket.reduceOnly}
              disabled={busy}
              onChange={(e) => patchTicket({ reduceOnly: e.target.checked })}
            />
            reduce-only
          </label>
        </div>

        {/* estimated premium — exact integer ticks */}
        <div className="flex items-baseline justify-between rounded-sm border border-edge bg-bg/50 px-2 py-1 font-mono text-[11px] tabular-nums">
          <span className="font-sans text-[9px] font-semibold tracking-[0.14em] text-txt-dim uppercase">
            est premium
          </span>
          {premium !== null ? (
            <span>
              {fmtMoney(premium)}{' '}
              <span className="text-txt-dim">· {fmtInt(premium)}t</span>
            </span>
          ) : (
            <span className="text-txt-dim">—</span>
          )}
        </div>

        <button
          type="button"
          onClick={() => void submit()}
          disabled={!canSubmit}
          title={noSubmitReason ?? undefined}
          className={`btn h-9 text-[11px] ${ticket.side === 'buy' ? 'btn-buy' : 'btn-sell'}`}
        >
          {busy
            ? 'sending'
            : ticket.kind === 'market'
              ? `${ticket.side} ${qtyValid ? qty : '—'} market`
              : `${ticket.side} ${qtyValid ? qty : '—'} @ ${priceValid ? fmtTicks(price!) : '—'}`}
        </button>

        {/* inline result */}
        {result?.t === 'ok' && (
          <div
            className="rounded-sm border border-up/40 bg-up/10 p-2 font-mono text-[11px] tabular-nums"
            role="status"
          >
            <p className="font-semibold text-up-bright">
              ACK #{result.orderId} · leaves {result.leaves}
            </p>
            {result.fills.length === 0 ? (
              <p className="text-txt-dim">no fills — resting on the book</p>
            ) : (
              result.fills.map((f, i) => (
                <p key={i}>
                  fill {f.qty} @ {fmtTicks(f.price)}
                  {f.fee > 0 ? ` · fee ${f.fee}t` : ''}
                  {f.rebate > 0 ? ` · rebate ${f.rebate}t` : ''}
                </p>
              ))
            )}
          </div>
        )}
        {result?.t === 'reject' && (
          <div
            className="rounded-sm border border-down/40 bg-down/10 p-2 text-[11px] leading-snug"
            role="alert"
          >
            <span className="font-bold tracking-wide text-down-bright uppercase">rejected</span>{' '}
            <span className="text-txt">{REJECT_HUMAN[result.reason]}</span>{' '}
            <span className="font-mono text-[10px] text-txt-dim">({result.reason} · code {result.code})</span>
          </div>
        )}
        {result?.t === 'error' && (
          <div
            className="rounded-sm border border-down/40 bg-down/10 p-2 text-[11px] leading-snug text-down-bright"
            role="alert"
          >
            {result.msg}
          </div>
        )}
      </div>
    </Panel>
  );
}

function SideButton({
  side,
  active,
  onClick,
}: {
  side: Side;
  active: boolean;
  onClick: () => void;
}) {
  const cls =
    side === 'buy'
      ? 'border-up/60 bg-up/20 text-up-bright'
      : 'border-down/60 bg-down/20 text-down-bright';
  return (
    <button
      type="button"
      onClick={onClick}
      aria-pressed={active}
      className={`h-8 rounded-sm border text-[11px] font-bold tracking-[0.14em] uppercase ${
        active ? cls : 'border-edge text-txt-dim hover:bg-edge/50'
      }`}
    >
      {side}
    </button>
  );
}

function KindButton({
  kind,
  active,
  onClick,
}: {
  kind: OrderKind;
  active: boolean;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      aria-pressed={active}
      className={`h-7 rounded-sm border text-[10px] font-bold tracking-[0.14em] uppercase ${
        active
          ? 'border-accent/50 bg-accent/15 text-accent-bright'
          : 'border-edge text-txt-dim hover:bg-edge/50'
      }`}
    >
      {kind === 'market' ? 'market + protection' : 'limit'}
    </button>
  );
}
