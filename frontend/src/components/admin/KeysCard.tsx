'use client';

/**
 * KeysCard — mints a new API key (contract §2.4 POST /admin/keys). The secret
 * is shown exactly once, with a copy button and a warning; it is never
 * stored, never logged and can be hidden after saving.
 */
import { useState } from 'react';
import { useTerminal } from '@/providers/TerminalProvider';
import { useStoreSection } from '@/lib/store';
import { useToast } from '@/components/Toast';
import { parseUInt } from '@/lib/format';
import { describeError } from '@/lib/messages';
import type { KeyMintResp, Scope } from '@/lib/types';
import { Panel } from '@/components/Panel';
import { Field } from '@/components/Field';

const SCOPES: Scope[] = ['read', 'trade', 'admin'];
const TIERS = ['base', 'silver', 'gold', 'prime'] as const;

export function KeysCard() {
  const { client, store } = useTerminal();
  const { push } = useToast();
  useStoreSection(store, 'conn');

  const [accountStr, setAccountStr] = useState('');
  const [scope, setScope] = useState<Scope>('trade');
  const [tier, setTier] = useState<string>('base');
  const [busy, setBusy] = useState(false);
  const [minted, setMinted] = useState<KeyMintResp | null>(null);
  const [copied, setCopied] = useState(false);

  const account = parseUInt(accountStr);

  async function mint(): Promise<void> {
    if (account === null || busy) return;
    setBusy(true);
    setMinted(null);
    setCopied(false);
    try {
      const res = await client.mintKey({ account, scope, tier });
      setMinted(res);
      push('success', `Key ${res.key_id} minted`, `scope ${res.scope} · account ${res.account} — secret shown once`);
    } catch (e) {
      push('error', 'Key mint failed', describeError(e));
    } finally {
      setBusy(false);
    }
  }

  async function copySecret(): Promise<void> {
    if (minted === null) return;
    const ok = await copyText(minted.secret);
    if (ok) {
      setCopied(true);
      push('info', 'Secret copied to clipboard', 'Paste it into a secure store — it will not be shown again.');
      setTimeout(() => setCopied(false), 2500);
    } else {
      push('warn', 'Copy failed', 'Select the secret text and copy it manually.');
    }
  }

  return (
    <Panel title="API keys" className="h-full min-h-0" bodyClassName="overflow-y-auto">
      <div className="flex flex-col gap-2 p-2">
        <div className="grid grid-cols-3 gap-2">
          <Field label="Account" error={accountStr !== '' && account === null ? 'unsigned integer required' : null}>
            <input
              className="field"
              value={accountStr}
              onChange={(e) => setAccountStr(e.target.value)}
              inputMode="numeric"
              placeholder="7"
              disabled={busy}
              aria-label="Key account"
            />
          </Field>
          <Field label="Scope">
            <select
              className="field"
              value={scope}
              onChange={(e) => setScope(e.target.value as Scope)}
              disabled={busy}
              aria-label="Key scope"
            >
              {SCOPES.map((s) => (
                <option key={s} value={s}>
                  {s}
                </option>
              ))}
            </select>
          </Field>
          <Field label="Tier">
            <select
              className="field"
              value={tier}
              onChange={(e) => setTier(e.target.value)}
              disabled={busy}
              aria-label="Key tier"
            >
              {TIERS.map((t) => (
                <option key={t} value={t}>
                  {t}
                </option>
              ))}
            </select>
          </Field>
        </div>

        <button
          type="button"
          className="btn btn-accent"
          disabled={account === null || busy || store.offline}
          onClick={() => void mint()}
        >
          {busy ? 'minting' : 'mint key'}
        </button>

        {minted !== null && (
          <div className="rounded-sm border border-warn/40 bg-warn/10 p-2" role="alert">
            <p className="font-sans text-[9px] font-bold tracking-[0.14em] text-warn-bright uppercase">
              secret — shown once
            </p>
            <p className="mt-1 text-[10px] leading-relaxed text-txt-dim">
              Copy it now. The venue will never return this secret again and this terminal never
              stores it anywhere.
            </p>
            <dl className="mt-2 flex flex-col gap-0.5 font-mono text-[11px] tabular-nums">
              <div className="flex gap-2">
                <dt className="w-16 shrink-0 text-txt-dim">key id</dt>
                <dd className="text-txt">{minted.key_id}</dd>
              </div>
              <div className="flex gap-2">
                <dt className="w-16 shrink-0 text-txt-dim">scope</dt>
                <dd className="text-txt">
                  {minted.scope} · account {minted.account}
                </dd>
              </div>
            </dl>
            <code className="mt-2 block rounded-sm border border-edge bg-bg p-2 font-mono text-[10px] leading-relaxed break-all select-all">
              {minted.secret}
            </code>
            <div className="mt-2 flex gap-2">
              <button type="button" className="btn btn-ghost" onClick={() => void copySecret()}>
                {copied ? 'copied' : 'copy secret'}
              </button>
              <button type="button" className="btn btn-ghost" onClick={() => setMinted(null)}>
                i saved it — hide
              </button>
            </div>
          </div>
        )}
      </div>
    </Panel>
  );
}

/** Clipboard with a textarea fallback for non-secure contexts. */
async function copyText(text: string): Promise<boolean> {
  try {
    if (typeof navigator !== 'undefined' && navigator.clipboard?.writeText) {
      await navigator.clipboard.writeText(text);
      return true;
    }
  } catch {
    /* fall through to the legacy path */
  }
  try {
    const ta = document.createElement('textarea');
    ta.value = text;
    ta.style.position = 'fixed';
    ta.style.opacity = '0';
    document.body.appendChild(ta);
    ta.select();
    const ok = document.execCommand('copy');
    document.body.removeChild(ta);
    return ok;
  } catch {
    return false;
  }
}
