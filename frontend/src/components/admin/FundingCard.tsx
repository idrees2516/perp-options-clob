'use client';

/**
 * FundingCard — admin account + funding operations (contract §2.4):
 * create account (id + tier), deposit / withdraw (account, underlying,
 * integer tick amount). Money movement requires an explicit ConfirmDialog.
 */
import { useState } from 'react';
import { useTerminal } from '@/providers/TerminalProvider';
import { useStoreSection } from '@/lib/store';
import { useToast } from '@/components/Toast';
import { fmtInt, fmtMoney, parseUInt } from '@/lib/format';
import { describeError } from '@/lib/messages';
import { Panel } from '@/components/Panel';
import { ConfirmDialog } from '@/components/ConfirmDialog';
import { Field } from '@/components/Field';

const TIERS = ['base', 'silver', 'gold', 'prime'] as const;

type Pending = { mode: 'deposit' | 'withdraw'; account: number; underlying: number; amount: number };

export function FundingCard() {
  const { client, store, refreshAccount } = useTerminal();
  const { push } = useToast();
  useStoreSection(store, 'conn');

  const [newAccountStr, setNewAccountStr] = useState('');
  const [newTier, setNewTier] = useState<string>('base');
  const [accountStr, setAccountStr] = useState('');
  const [underlyingStr, setUnderlyingStr] = useState('0');
  const [amountStr, setAmountStr] = useState('');
  const [busy, setBusy] = useState(false);
  const [pending, setPending] = useState<Pending | null>(null);
  const [status, setStatus] = useState<string | null>(null);

  const newAccount = parseUInt(newAccountStr);
  const account = parseUInt(accountStr);
  const underlying = parseUInt(underlyingStr, 4);
  const amount = parseUInt(amountStr, 12);
  const amountValid = amount !== null && amount > 0;

  async function run(fn: () => Promise<unknown>, okMsg: string): Promise<boolean> {
    setBusy(true);
    try {
      await fn();
      push('success', okMsg);
      setStatus(`ok — ${okMsg}`);
      return true;
    } catch (e) {
      const d = describeError(e);
      push('error', 'Admin action failed', d);
      setStatus(`error — ${d}`);
      return false;
    } finally {
      setBusy(false);
    }
  }

  async function createAccount(): Promise<void> {
    if (newAccount === null || busy) return;
    const ok = await run(
      () => client.createAccount(newAccount, newTier),
      `Account ${newAccount} created (${newTier})`,
    );
    if (ok) setNewAccountStr('');
  }

  function requestFunding(mode: 'deposit' | 'withdraw'): void {
    if (account === null || underlying === null || !amountValid || busy) return;
    setPending({ mode, account, underlying, amount });
  }

  async function confirmFunding(): Promise<void> {
    if (pending === null) return;
    const { mode, account: a, underlying: u, amount: amt } = pending;
    setBusy(true);
    const ok = await run(
      () =>
        mode === 'deposit'
          ? client.deposit({ account: a, underlying: u, amount: amt })
          : client.withdraw({ account: a, underlying: u, amount: amt }),
      `${mode === 'deposit' ? 'Deposited' : 'Withdrew'} ${fmtMoney(amt)} ${mode === 'deposit' ? 'to' : 'from'} account ${a} (U${u})`,
    );
    if (ok) {
      setAmountStr('');
      refreshAccount();
    }
    setPending(null);
    setBusy(false);
  }

  return (
    <Panel title="Funding & accounts" className="h-full min-h-0" bodyClassName="overflow-y-auto">
      <div className="flex flex-col gap-3 p-2">
        {/* create account */}
        <div className="grid grid-cols-[minmax(0,1fr)_110px_auto] items-end gap-2">
          <Field label="New account id" error={newAccountStr !== '' && newAccount === null ? 'unsigned integer required' : null}>
            <input
              className="field"
              value={newAccountStr}
              onChange={(e) => setNewAccountStr(e.target.value)}
              inputMode="numeric"
              placeholder="7"
              disabled={busy}
              aria-label="New account id"
            />
          </Field>
          <Field label="Tier">
            <select
              className="field"
              value={newTier}
              onChange={(e) => setNewTier(e.target.value)}
              disabled={busy}
              aria-label="Account tier"
            >
              {TIERS.map((t) => (
                <option key={t} value={t}>
                  {t}
                </option>
              ))}
            </select>
          </Field>
          <button
            type="button"
            className="btn btn-accent"
            disabled={newAccount === null || busy || store.offline}
            onClick={() => void createAccount()}
          >
            create
          </button>
        </div>

        <div className="border-t border-edge" />

        {/* deposit / withdraw */}
        <div className="grid grid-cols-2 gap-2">
          <Field label="Account id" error={accountStr !== '' && account === null ? 'unsigned integer required' : null}>
            <input
              className="field"
              value={accountStr}
              onChange={(e) => setAccountStr(e.target.value)}
              inputMode="numeric"
              placeholder="7"
              disabled={busy}
              aria-label="Account id"
            />
          </Field>
          <Field label="Underlying" error={underlyingStr !== '' && underlying === null ? 'unsigned integer required' : null}>
            <input
              className="field"
              value={underlyingStr}
              onChange={(e) => setUnderlyingStr(e.target.value)}
              inputMode="numeric"
              placeholder="0"
              disabled={busy}
              aria-label="Underlying id"
            />
          </Field>
        </div>
        <Field
          label="Amount (ticks)"
          hint={amountValid ? `${fmtInt(amount)} ticks = ${fmtMoney(amount)}` : 'integer ticks, u64'}
          error={amountStr !== '' && !amountValid ? 'positive integer required' : null}
        >
          <input
            className="field"
            value={amountStr}
            onChange={(e) => setAmountStr(e.target.value)}
            inputMode="numeric"
            placeholder="1000000"
            disabled={busy}
            aria-label="Amount in ticks"
          />
        </Field>
        <div className="grid grid-cols-2 gap-2">
          <button
            type="button"
            className="btn btn-buy"
            disabled={account === null || underlying === null || !amountValid || busy || store.offline}
            onClick={() => requestFunding('deposit')}
          >
            deposit
          </button>
          <button
            type="button"
            className="btn btn-sell"
            disabled={account === null || underlying === null || !amountValid || busy || store.offline}
            onClick={() => requestFunding('withdraw')}
          >
            withdraw
          </button>
        </div>

        {status && (
          <p className="rounded-sm border border-edge bg-bg/40 px-2 py-1 font-mono text-[10px] leading-snug break-words text-txt-dim">
            {status}
          </p>
        )}
      </div>

      <ConfirmDialog
        open={pending !== null}
        title={pending?.mode === 'withdraw' ? 'Withdraw funds' : 'Deposit funds'}
        confirmLabel={pending?.mode === 'withdraw' ? 'WITHDRAW' : 'DEPOSIT'}
        danger={pending?.mode === 'withdraw'}
        busy={busy}
        onConfirm={() => void confirmFunding()}
        onClose={() => setPending(null)}
      >
        {pending !== null && (
          <div className="font-mono text-[11px] tabular-nums">
            <p>
              account <span className="text-txt">{pending.account}</span> · underlying{' '}
              <span className="text-txt">U{pending.underlying}</span>
            </p>
            <p className="mt-1">
              amount{' '}
              <span className="text-txt">
                {fmtInt(pending.amount)} ticks = {fmtMoney(pending.amount)}
              </span>
            </p>
            <p className="mt-2 text-[10px]">
              {pending.mode === 'withdraw'
                ? 'Funds are removed immediately — balances may go negative on open positions.'
                : 'Funds are credited immediately and become available balance.'}
            </p>
          </div>
        )}
      </ConfirmDialog>
    </Panel>
  );
}
