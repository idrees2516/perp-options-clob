'use client';

import type { SymbolLive } from '@/lib/store';
import { fmtHM, fmtTicks } from '@/lib/format';

/** Compact human label for an option symbol, e.g. "C 5000.00 16:00". */
export function symbolLabel(sym: SymbolLive | undefined): string {
  if (!sym) return '—';
  return `${sym.kind === 'call' ? 'C' : 'P'} ${fmtTicks(sym.strike)} ${fmtHM(sym.expiry_ms)}`;
}
