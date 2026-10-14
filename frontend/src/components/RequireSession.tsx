'use client';

import { useEffect } from 'react';
import type { ReactNode } from 'react';
import { useRouter } from 'next/navigation';
import { useSession } from '@/providers/SessionProvider';
import { useToast } from '@/components/Toast';

function BootScreen({ label }: { label: string }) {
  return (
    <div className="flex min-h-screen items-center justify-center bg-bg">
      <p
        className="text-xs tracking-[0.2em] text-txt-dim uppercase"
        role="status"
        aria-live="polite"
      >
        {label}
      </p>
    </div>
  );
}

/** Gate for authenticated routes; redirects to / when the session is gone. */
export function RequireSession({ children }: { children: ReactNode }) {
  const { session, hydrated } = useSession();
  const router = useRouter();

  useEffect(() => {
    if (hydrated && !session) void router.replace('/');
  }, [hydrated, session, router]);

  if (!hydrated) return <BootScreen label="Restoring session" />;
  if (!session) return <BootScreen label="Redirecting to login" />;
  return <>{children}</>;
}

/** Gate for the admin console — scope=admin keys only. */
export function RequireAdmin({ children }: { children: ReactNode }) {
  const { session } = useSession();
  const router = useRouter();
  const { push } = useToast();

  useEffect(() => {
    if (session && session.scope !== 'admin') {
      push('warn', 'Admin scope required', 'This key cannot open the admin console.');
      void router.replace('/trade');
    }
  }, [session, router, push]);

  if (!session || session.scope !== 'admin') return <BootScreen label="Checking key scope" />;
  return <>{children}</>;
}
