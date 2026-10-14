'use client';

import { createContext, useCallback, useContext, useEffect, useMemo, useState } from 'react';
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

const SessionContext = createContext<SessionContextValue | null>(null);

/**
 * Session lives in sessionStorage ONLY (per-tab, never written to disk).
 * The secret is handed to the API layer for HMAC signing and is never
 * logged or rendered in clear text.
 */
export function SessionProvider({ children }: { children: ReactNode }) {
  const [session, setSession] = useState<SessionRecord | null>(null);
  const [hydrated, setHydrated] = useState(false);

  useEffect(() => {
    setSession(loadSession());
    setHydrated(true);
  }, []);

  const login = useCallback((rec: SessionRecord) => {
    saveSession(rec);
    setSession(rec);
  }, []);

  const logout = useCallback(() => {
    clearSession();
    setSession(null);
  }, []);

  const updateScope = useCallback((scope: Scope) => {
    updateSessionScope(scope);
    setSession((prev) => (prev && prev.scope !== scope ? { ...prev, scope } : prev));
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
