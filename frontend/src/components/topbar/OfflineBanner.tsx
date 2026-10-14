'use client';

import { useTerminal } from '@/providers/TerminalProvider';
import { useStoreSection } from '@/lib/store';

/**
 * Engine reachability banner. "Engine unreachable — retrying" whenever the
 * REST surface is down (order entry is disabled then); a softer banner when
 * only the market-data socket is reconnecting. Data is NEVER faked.
 */
export function OfflineBanner() {
  const { store } = useTerminal();
  useStoreSection(store, 'conn');

  if (store.offline) {
    return (
      <div
        role="alert"
        className="flex shrink-0 items-center justify-center gap-2 border-b border-down-bright/40 bg-down/10 px-3 py-1.5"
      >
        <span className="h-2 w-2 animate-pulse rounded-full bg-down-bright" aria-hidden />
        <span className="text-[11px] font-bold tracking-[0.18em] text-down-bright uppercase">
          Engine unreachable — retrying
        </span>
        <span className="hidden text-[10px] text-txt-dim sm:inline">order entry disabled</span>
      </div>
    );
  }
  if (store.connStatus === 'reconnecting' || store.connStatus === 'connecting') {
    return (
      <div
        role="status"
        className="flex shrink-0 items-center justify-center gap-2 border-b border-warn-bright/30 bg-warn/10 px-3 py-1"
      >
        <span className="h-2 w-2 animate-pulse rounded-full bg-warn-bright" aria-hidden />
        <span className="text-[11px] font-semibold tracking-[0.18em] text-warn-bright uppercase">
          Market data reconnecting
        </span>
      </div>
    );
  }
  return null;
}
