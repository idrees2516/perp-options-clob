'use client';

/**
 * VenueHealthCard — polls /metrics every 2s (admin scope) and renders the
 * per-shard table (orders / trades / tps_ema / queue_depth / journal_bytes /
 * state_hash), gateway stats, the conservation badge and a gateway-TPS
 * sparkline fed by a ring buffer held in component state.
 */
import { useEffect, useRef, useState } from 'react';
import { useTerminal } from '@/providers/TerminalProvider';
import { usePoll } from '@/hooks/usePoll';
import { RingBuffer, Sparkline } from '@/components/Sparkline';
import { fmtBytes, fmtInt } from '@/lib/format';
import { Panel } from '@/components/Panel';
import { TableSkeleton } from '@/components/Skeleton';
import { Badge } from '@/components/StatusDot';

const SAMPLES = 90; // 3 minutes of 2s polls

export function VenueHealthCard() {
  const { client } = useTerminal();
  const { data, error, refresh } = usePoll(() => client.getMetrics(), 2000);

  // Ring buffer mutated only inside the effect; the rendered series lives in
  // state so nothing reads a ref during render (react-hooks/refs).
  const ring = useRef(new RingBuffer(SAMPLES));
  const [values, setValues] = useState<number[]>([]);
  useEffect(() => {
    if (data === null) return;
    const tps = Number(data.gateway.tps);
    // Only finite samples enter the series — a backend without the gateway
    // TPS field must not poison the sparkline with NaN points.
    if (Number.isFinite(tps)) {
      ring.current.push(tps);
      setValues(ring.current.values());
    }
  }, [data]);

  return (
    <Panel
      title="Venue health"
      right={
        data !== null ? (
          <Badge tone={data.conservation ? 'up' : 'down'}>
            {data.conservation ? 'conservation ok' : 'conservation violated'}
          </Badge>
        ) : undefined
      }
      className="h-full min-h-0"
      bodyClassName="overflow-y-auto"
    >
      <div className="flex flex-col gap-2 p-2">
        {error !== null && (
          <p
            role="alert"
            className="rounded-sm border border-warn/40 bg-warn/10 px-2 py-1.5 text-[11px] leading-snug text-warn-bright"
          >
            metrics unavailable — {error}{' '}
            <button type="button" className="btn btn-ghost ml-1 h-5 px-1.5 text-[9px]" onClick={refresh}>
              retry
            </button>
          </p>
        )}

        {data === null && error === null && <TableSkeleton rows={4} cols={6} label="loading metrics" />}

        {data !== null && (
          <>
            {/* gateway stats + TPS sparkline */}
            <div className="flex flex-wrap items-center gap-3 rounded-sm border border-edge bg-bg/40 p-2">
              <div className="flex flex-col gap-0.5 font-mono text-[10px] tabular-nums text-txt-dim">
                <span>
                  gateway tps{' '}
                  <span className="text-accent-bright">{fmtRate(data.gateway.tps)}</span>
                </span>
                <span>
                  conns <span className="text-txt">{fmtInt(data.gateway.conns)}</span> · ws{' '}
                  <span className="text-txt">{fmtInt(data.gateway.ws)}</span>
                </span>
                <span>
                  rx <span className="text-txt">{fmtInt(data.gateway.rx)}</span> · tx{' '}
                  <span className="text-txt">{fmtInt(data.gateway.tx)}</span>
                </span>
                <span>
                  auth fails{' '}
                  <span className={data.gateway.auth_fails > 0 ? 'text-warn-bright' : 'text-txt'}>
                    {fmtInt(data.gateway.auth_fails)}
                  </span>{' '}
                  · rate limited{' '}
                  <span className={data.gateway.rate_limited > 0 ? 'text-warn-bright' : 'text-txt'}>
                    {fmtInt(data.gateway.rate_limited)}
                  </span>
                </span>
              </div>
              <div className="ml-auto text-accent-bright">
                <Sparkline values={values} width={260} height={44} />
              </div>
            </div>

            {/* per-shard table */}
            <div className="overflow-x-auto">
              <table className="w-full font-mono text-[11px] tabular-nums">
                <thead>
                  <tr className="font-sans text-[9px] font-semibold tracking-[0.1em] text-txt-dim uppercase">
                    <th className="px-1 py-1 text-left font-semibold">shard</th>
                    <th className="px-1 py-1 text-left font-semibold">und</th>
                    <th className="px-1 py-1 text-right font-semibold">orders</th>
                    <th className="px-1 py-1 text-right font-semibold">trades</th>
                    <th className="px-1 py-1 text-right font-semibold">tps ema</th>
                    <th className="px-1 py-1 text-right font-semibold">queue</th>
                    <th className="px-1 py-1 text-right font-semibold">journal</th>
                    <th className="px-1 py-1 text-left font-semibold">state hash</th>
                  </tr>
                </thead>
                <tbody>
                  {data.shards.map((s) => (
                    <tr key={s.shard} className="border-b border-edge/30 hover:bg-edge/20">
                      <td className="px-1 py-1 text-txt-dim">{s.shard}</td>
                      <td className="px-1 py-1 text-txt-dim">U{s.underlying}</td>
                      <td className="px-1 py-1 text-right">{fmtInt(s.orders)}</td>
                      <td className="px-1 py-1 text-right">{fmtInt(s.trades)}</td>
                      <td className="px-1 py-1 text-right">{fmtRate(s.tps_ema)}</td>
                      <td
                        className={`px-1 py-1 text-right ${s.queue_depth > 0 ? 'text-warn-bright' : 'text-txt-dim'}`}
                      >
                        {fmtInt(s.queue_depth)}
                      </td>
                      <td className="px-1 py-1 text-right text-txt-dim">{fmtBytes(s.journal_bytes)}</td>
                      <td className="px-1 py-1 text-left text-txt-dim" title={s.state_hash}>
                        {s.state_hash.slice(0, 10)}
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          </>
        )}
      </div>
    </Panel>
  );
}

/** Rates are engine telemetry, not money — one decimal is display-only. */
function fmtRate(v: number): string {
  return Number.isFinite(v) ? v.toFixed(1) : '—';
}
