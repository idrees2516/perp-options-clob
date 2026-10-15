'use client';

import { createContext, useCallback, useContext, useMemo, useSyncExternalStore } from 'react';
import type { ReactNode } from 'react';
import type { Scope } from '@/lib/types';
import {
  clearSession,
  loadSession,
  saveSession,
  updateSessionScope,
  type SessionRecord,
} from '@/lib/session';

interface SessionContextValue {
  hydrated: boolean;
  session: SessionRecord | null;
  login(rec: SessionRecord): void;
  logout(): void;
  updateScope(scope: Scope): void;
}

/**
 * Session lives in sessionStorage ONLY (per-tab, never written to disk).
 * The secret is handed to the API layer for HMAC signing and is never
 * logged or rendered in clear text.
 *
 * The store below is the single source of truth on the client:
 * `useSyncExternalStore` subscribes to it, so login/logout from ANY
 * component re-renders every consumer — no setState-in-effect hydration
 * dance, no stale copies.
 */

const listeners = new Set<() => void>();

function subscribe(fn: () => void): () => void {
  listeners.add(fn);
  return () => {
    listeners.delete(fn);
  };
}

function emit(): void {
  for (const fn of listeners) fn();
}

/** Cached snapshot — `useSyncExternalStore` requires a stable reference. */
let cached: SessionRecord | null | undefined;

function snapshot(): SessionRecord | null {
  if (cached === undefined) cached = loadSession();
  return cached;
}

function writeSession(next: SessionRecord | null): void {
  if (next === null) clearSession();
  else saveSession(next);
  cached = next;
  emit();
}

const SessionContext = createContext<SessionContextValue | null>(null);

export function SessionProvider({ children }: { children: ReactNode }) {
  const session = useSyncExternalStore(subscribe, snapshot, () => null);
  // true on the client, false during SSR/hydration — no effect needed.
  const hydrated = useSyncExternalStore(
    () => () => undefined,
    () => true,
    () => false,
  );

  const login = useCallback((rec: SessionRecord) => {
    writeSession(rec);
  }, []);

  const logout = useCallback(() => {
    writeSession(null);
  }, []);

  const updateScope = useCallback((scope: Scope) => {
    updateSessionScope(scope);
    cached = undefined; // re-read the mutated record
    emit();
  }, []);

  const value = useMemo(
    () => ({ hydrated, session, login, logout, updateScope }),
    [hydrated, session, login, logout, updateScope],
  );

  return <SessionContext.Provider value={value}>{children}</SessionContext.Provider>;
}

export function useSession(): SessionContextValue {
  const ctx = useContext(SessionContext);
  if (!ctx) throw new Error('useSession must be used inside <SessionProvider>');
  return ctx;
}
