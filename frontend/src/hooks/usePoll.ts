'use client';

import { useCallback, useEffect, useRef, useState } from 'react';
import { describeError } from '@/lib/messages';

/**
 * setTimeout-chained poller (no pile-up, skips hidden tabs).
 * `fetcher` may change identity freely — the latest one is always used.
 */
export function usePoll<T>(
  fetcher: () => Promise<T>,
  intervalMs: number,
): {
  data: T | null;
  error: string | null;
  refresh: () => void;
} {
  const [data, setData] = useState<T | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [nonce, setNonce] = useState(0);
  const fetcherRef = useRef(fetcher);
  // Keep the ref current without touching it during render (react-hooks/refs):
  // a dependency-free effect runs after every render, before the poller reads it.
  useEffect(() => {
    fetcherRef.current = fetcher;
  });

  useEffect(() => {
    let alive = true;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const run = async (): Promise<void> => {
      if (typeof document === 'undefined' || !document.hidden) {
        try {
          const d = await fetcherRef.current();
          if (alive) {
            setData(d);
            setError(null);
          }
        } catch (e) {
          if (alive) setError(describeError(e));
        }
      }
      if (alive) timer = setTimeout(run, intervalMs);
    };
    void run();
    return () => {
      alive = false;
      if (timer !== undefined) clearTimeout(timer);
    };
  }, [intervalMs, nonce]);

  const refresh = useCallback(() => setNonce((n) => n + 1), []);
  return { data, error, refresh };
}
