'use client';

/**
 * Login screen — key id + secret + venue base URL.
 *
 * Verification: a signed `GET /v1/account` (contract §2.2) proves both the
 * credentials and the account/tier. Key scope comes from the WebSocket
 * `hello` frame (contract §3) via a short one-shot probe; if the socket is
 * unavailable we fall back to an admin-probe of `/admin/audit`.
 *
 * SECURITY: the secret is typed into a masked password field, handed to the
 * HMAC signer, persisted ONLY in sessionStorage (session.ts) and never
 * logged or rendered in clear text.
 */
import { useEffect, useState } from 'react';
import type { FormEvent } from 'react';
import { useRouter } from 'next/navigation';
import { ApiError, DEFAULT_BASE_URL, VenueClient, WsManager } from '@/lib/api';
import { isHex, normalizeBaseUrl, type SessionRecord } from '@/lib/session';
import { describeError } from '@/lib/messages';
import { SCOPE_DESC } from '@/lib/messages';
import { useSession } from '@/providers/SessionProvider';
import { useToast } from '@/components/Toast';
import { Field } from '@/components/Field';
import { StatusDot } from '@/components/StatusDot';
import type { Scope } from '@/lib/types';

export default function LoginPage() {
  const router = useRouter();
  const { session, hydrated, login } = useSession();
  const { push } = useToast();

  const [keyId, setKeyId] = useState('');
  const [secret, setSecret] = useState('');
  const [baseUrlInput, setBaseUrlInput] = useState(DEFAULT_BASE_URL);
  const [busy, setBusy] = useState(false);
  const [phase, setPhase] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  // Already signed in (same tab) — go straight to the terminal.
  useEffect(() => {
    if (hydrated && session) void router.replace('/trade');
  }, [hydrated, session, router]);

  async function onSubmit(e: FormEvent<HTMLFormElement>): Promise<void> {
    e.preventDefault();
    if (busy) return;

    const kid = keyId.trim().toLowerCase();
    const sec = secret.trim().toLowerCase();
    if (!isHex(kid, 8)) {
      setError('Key id must be exactly 8 lowercase hex characters (e.g. a1b2c3d4)');
      return;
    }
    if (!isHex(sec, 64)) {
      setError('API secret must be exactly 64 lowercase hex characters');
      return;
    }
    const baseUrl = normalizeBaseUrl(baseUrlInput);
    if (baseUrl === null) {
      setError('Venue URL must be a valid http(s) URL (e.g. http://localhost:8080)');
      return;
    }

    setBusy(true);
    setError(null);
    setPhase('Contacting engine');
    try {
      const client = new VenueClient({ keyId: kid, secretHex: sec, baseUrl });
      try {
        await client.syncTime(); // keeps signing timestamps inside the 5s window
      } catch {
        /* clock fallback: the client retries once on StaleTimestamp */
      }

      setPhase('Verifying key');
      const acc = await client.getAccount();

      setPhase('Checking key scope');
      const scope = await resolveScope(client);

      const rec: SessionRecord = {
        keyId: kid,
        secretHex: sec,
        baseUrl,
        account: acc.account,
        tier: acc.tier,
        scope,
      };
      login(rec);
      push('success', `Signed in — account ${acc.account}`, `${SCOPE_DESC[scope]} · ${acc.tier} tier`);
      void router.replace('/trade');
    } catch (e) {
      setError(describeError(e));
    } finally {
      setBusy(false);
      setPhase(null);
    }
  }

  return (
    <main className="flex min-h-dvh flex-col items-center justify-center bg-bg px-4 py-10 text-txt">
      <div className="w-full max-w-sm">
        <header className="mb-6 text-center">
          <h1 className="text-xl font-semibold tracking-wide">
            ns-venue <span className="text-txt-dim">/ 0DTE terminal</span>
          </h1>
          <p className="mt-1 text-[11px] text-txt-dim">
            Sign in with an API key issued by the venue
          </p>
        </header>

        <form
          onSubmit={onSubmit}
          noValidate
          className="flex flex-col gap-3 rounded border border-edge bg-panel p-4 shadow-lg shadow-black/40"
        >
          <Field label="Key id">
            <input
              className="field"
              value={keyId}
              onChange={(e) => setKeyId(e.target.value)}
              placeholder="a1b2c3d4"
              autoComplete="off"
              autoCapitalize="off"
              spellCheck={false}
              maxLength={8}
              disabled={busy}
              aria-label="API key id"
            />
          </Field>

          <Field
            label="API secret"
            hint="64 lowercase hex characters — masked, never logged, stored only in this tab"
          >
            <input
              className="field"
              type="password"
              value={secret}
              onChange={(e) => setSecret(e.target.value)}
              placeholder="64 hex characters"
              autoComplete="off"
              spellCheck={false}
              maxLength={64}
              disabled={busy}
              aria-label="API secret"
            />
          </Field>

          <Field label="Venue URL">
            <input
              className="field"
              value={baseUrlInput}
              onChange={(e) => setBaseUrlInput(e.target.value)}
              placeholder="http://localhost:8080"
              autoCapitalize="off"
              spellCheck={false}
              disabled={busy}
              aria-label="Venue base URL"
            />
          </Field>

          {error && (
            <p
              role="alert"
              className="rounded-sm border border-down/40 bg-down/10 px-2 py-1.5 text-[11px] leading-snug text-down-bright"
            >
              {error}
            </p>
          )}

          <button type="submit" disabled={busy} className="btn btn-accent h-9 text-[11px]">
            {busy ? (
              <span className="flex items-center gap-2">
                <StatusDot state="warn" />
                {(phase ?? 'Signing in').toLowerCase()}…
              </span>
            ) : (
              'Sign in'
            )}
          </button>
        </form>

        <p className="mt-3 text-center text-[10px] leading-relaxed text-txt-dim">
          Credentials live only in this tab&apos;s sessionStorage. Admin keys unlock the venue
          console; trade keys open the order terminal.
        </p>
      </div>
    </main>
  );
}

// -- scope resolution ------------------------------------------------------------

/** One-shot WS probe: the `hello` frame carries the key's true scope. */
function probeWsScope(client: VenueClient): Promise<Scope | null> {
  return new Promise((resolve) => {
    let settled = false;
    let ws: WsManager | null = null;
    const finish = (s: Scope | null): void => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      try {
        ws?.close();
      } catch {
        /* already closing */
      }
      resolve(s);
    };
    const timer = setTimeout(() => finish(null), 3500);
    try {
      ws = new WsManager(
        client.creds,
        {
          onStatus: () => undefined,
          onMessage: (m) => {
            if (m.t === 'hello') finish(m.scope);
          },
        },
        () => client.venueNow(),
      );
      ws.connect();
    } catch {
      finish(null);
    }
  });
}

/**
 * REST fallback when the socket probe fails: only `/admin/audit` distinguishes
 * admin keys (200) from non-admin (403 ScopeDenied). Non-admin defaults to
 * 'trade'; the live hello frame corrects it within seconds on /trade.
 */
async function resolveScope(client: VenueClient): Promise<Scope> {
  const wsScope = await probeWsScope(client);
  if (wsScope !== null) return wsScope;
  try {
    await client.getAudit();
    return 'admin';
  } catch (e) {
    if (e instanceof ApiError && e.status === 401) throw e; // bad key — surface at login
    return 'trade';
  }
}
