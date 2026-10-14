'use client';

import { useMemo } from 'react';
import Link from 'next/link';
import { useRouter } from 'next/navigation';
import { useTerminal } from '@/providers/TerminalProvider';
import { useSession } from '@/providers/SessionProvider';
import { useStoreSection } from '@/lib/store';
import { fmtClock } from '@/lib/format';
import { StatusDot, type DotState } from '@/components/StatusDot';

export function TopBar() {
  const { store } = useTerminal();
  const { session, logout } = useSession();
  const router = useRouter();
  useStoreSection(store, 'conn');
  useStoreSection(store, 'clock');
  useStoreSection(store, 'account');

  const host = useMemo(() => {
    if (!session) return '';
    try {
      return new URL(session.baseUrl).host;
    } catch {
      return session.baseUrl;
    }
  }, [session]);

  const driftSec = Math.round(store.clockOffsetMs / 100) / 10;

  // TopBar only renders under <TerminalProvider>, which requires a session;
  // this guard exists purely for the type system.
  if (!session) return null;

  const dot: DotState = store.offline
    ? 'down'
    : store.connStatus === 'live'
      ? 'live'
      : store.connStatus === 'connecting' || store.connStatus === 'reconnecting'
        ? 'warn'
        : 'idle';
  const connLabel =
    store.connStatus === 'live'
      ? 'live'
      : store.connStatus === 'connecting'
        ? 'connecting'
        : store.connStatus === 'reconnecting'
          ? 'reconnecting'
          : store.connStatus === 'closed'
            ? 'closed'
            : 'idle';

  return (
    <header className="flex h-10 shrink-0 flex-wrap items-center justify-between gap-x-4 gap-y-1 border-b border-edge bg-panel px-3">
      <div className="flex items-center gap-3">
        <span className="text-sm font-semibold tracking-wide">
          ns-venue<span className="text-txt-dim"> / 0DTE</span>
        </span>
        <StatusDot state={dot} label={store.offline ? 'offline' : connLabel} />
        <span className="hidden font-mono text-[11px] text-txt-dim sm:inline">{host}</span>
      </div>

      <div className="flex items-center gap-2 font-mono text-xs tabular-nums">
        <span className="text-[10px] tracking-widest text-txt-dim uppercase">engine</span>
        <span>{fmtClock(store.serverNow())}</span>
        {Math.abs(driftSec) >= 0.1 && (
          <span
            className={`text-[10px] ${Math.abs(driftSec) > 2 ? 'text-warn-bright' : 'text-txt-dim'}`}
            title="local clock vs engine clock"
          >
            {driftSec > 0 ? '+' : ''}
            {driftSec.toFixed(1)}s
          </span>
        )}
      </div>

      <div className="flex items-center gap-2 text-[11px]">
        <span className="hidden font-mono text-txt-dim md:inline">
          acct <span className="text-txt">{session.account}</span> · {session.tier} ·{' '}
          <span className="text-txt">{session.scope}</span>
        </span>
        <nav className="flex items-center gap-1" aria-label="Primary">
          <Link
            href="/trade"
            className="rounded-sm px-2 py-1 text-[11px] font-semibold tracking-wide text-txt-dim uppercase hover:bg-edge hover:text-txt"
            aria-current="page"
          >
            Trade
          </Link>
          {session.scope === 'admin' && (
            <Link
              href="/admin"
              className="rounded-sm px-2 py-1 text-[11px] font-semibold tracking-wide text-txt-dim uppercase hover:bg-edge hover:text-txt"
            >
              Admin
            </Link>
          )}
        </nav>
        <button
          type="button"
          className="btn btn-ghost h-7 px-2"
          onClick={() => {
            logout();
            void router.replace('/');
          }}
        >
          LOGOUT
        </button>
      </div>
    </header>
  );
}
