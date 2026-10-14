/**
 * Number formatting & exact parsing utilities.
 *
 * RULES (contract §5): every money/price value is an integer tick count
 * (1 tick = 0.01). Display formats ticks/100 with 2 decimals; ALL arithmetic
 * on money stays in integer ticks — BigInt helpers below, never float
 * multiplication. Parsing of user input is string-based and exact.
 */

/** Integer ticks -> "1234.56" (2 decimals, signed). */
export function fmtTicks(ticks: number): string {
  if (!Number.isFinite(ticks)) return '—';
  const neg = ticks < 0;
  const a = Math.abs(Math.trunc(ticks));
  const whole = Math.floor(a / 100);
  const frac = a % 100;
  return `${neg ? '-' : ''}${whole}.${frac < 10 ? '0' : ''}${frac}`;
}

/** Integer ticks -> "+1.25" / "-0.75" (explicit sign). */
export function fmtTicksSigned(ticks: number): string {
  if (ticks === 0) return '0.00';
  return (ticks > 0 ? '+' : '') + fmtTicks(ticks);
}

/** Integer ticks -> "$1234.56" / "-$12.50". */
export function fmtMoney(ticks: number): string {
  if (!Number.isFinite(ticks)) return '—';
  return ticks < 0 ? `-$${fmtTicks(-ticks)}` : `$${fmtTicks(ticks)}`;
}

/** Integer with thousands separators: 1234567 -> "1,234,567". */
export function fmtInt(n: number): string {
  if (!Number.isFinite(n)) return '—';
  const neg = n < 0;
  const s = Math.trunc(Math.abs(n)).toString();
  let out = '';
  for (let i = 0; i < s.length; i++) {
    if (i > 0 && (s.length - i) % 3 === 0) out += ',';
    out += s[i]!;
  }
  return neg ? `-${out}` : out;
}

/** Signed qty: 5 -> "+5", -3 -> "-3". */
export function fmtQty(q: number): string {
  return q > 0 ? `+${q}` : String(q);
}

function pad2(n: number): string {
  return n < 10 ? `0${n}` : String(n);
}

/** "14:30:05" (local time). */
export function fmtClock(ms: number): string {
  const d = new Date(ms);
  return `${pad2(d.getHours())}:${pad2(d.getMinutes())}:${pad2(d.getSeconds())}`;
}

/** "14:30:05.123" (local time, millisecond precision for the tape). */
export function fmtClockMs(ms: number): string {
  const d = new Date(ms);
  const msec = d.getMilliseconds();
  return `${fmtClock(ms)}.${msec < 10 ? '00' : msec < 100 ? '0' : ''}${msec}`;
}

/** "16:00" — short expiry label (local time). */
export function fmtHM(ms: number): string {
  const d = new Date(ms);
  return `${pad2(d.getHours())}:${pad2(d.getMinutes())}`;
}

export interface Countdown {
  text: string;
  /** under 10 minutes — auction window territory */
  urgent: boolean;
  expired: boolean;
}

/** ms remaining -> "2h 14m 09s" / "14m 09s" / "09s" / "EXPIRED". */
export function fmtCountdown(msLeft: number): Countdown {
  if (msLeft <= 0) return { text: 'EXPIRED', urgent: false, expired: true };
  const totalSec = Math.floor(msLeft / 1000);
  const h = Math.floor(totalSec / 3600);
  const m = Math.floor((totalSec % 3600) / 60);
  const s = totalSec % 60;
  const text =
    h > 0 ? `${h}h ${pad2(m)}m ${pad2(s)}s` : m > 0 ? `${m}m ${pad2(s)}s` : `${pad2(s)}s`;
  return { text, urgent: msLeft < 600_000, expired: false };
}

/** Age: "3.2s" / "45s" / "12m" / "1h04m". */
export function fmtAge(ms: number): string {
  if (ms < 0 || !Number.isFinite(ms)) return '—';
  if (ms < 10_000) return `${(ms / 1000).toFixed(1)}s`;
  if (ms < 60_000) return `${Math.floor(ms / 1000)}s`;
  if (ms < 3_600_000) return `${Math.floor(ms / 60_000)}m`;
  const h = Math.floor(ms / 3_600_000);
  const m = Math.floor((ms % 3_600_000) / 60_000);
  return `${h}h${pad2(m)}m`;
}

/** Uptime / durations: "12m 34s" / "1h 02m". */
export function fmtDuration(ms: number): string {
  if (!Number.isFinite(ms) || ms < 0) return '—';
  const s = Math.floor(ms / 1000);
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const sec = s % 60;
  if (h > 0) return `${h}h ${pad2(m)}m`;
  if (m > 0) return `${m}m ${pad2(sec)}s`;
  return `${sec}s`;
}

/** Bytes: "325.4 MB". */
export function fmtBytes(n: number): string {
  if (!Number.isFinite(n) || n < 0) return '—';
  if (n < 1024) return `${n} B`;
  if (n < 1024 ** 2) return `${(n / 1024).toFixed(1)} KB`;
  if (n < 1024 ** 3) return `${(n / 1024 ** 2).toFixed(1)} MB`;
  return `${(n / 1024 ** 3).toFixed(2)} GB`;
}

// ---------------------------------------------------------------------------
// Exact parsing (strings in, integer ticks/qty out — no parseFloat anywhere)
// ---------------------------------------------------------------------------

/**
 * "2.5" -> 250, "2" -> 200, "2.50" -> 250.
 * Rejects: empty, sign, > 2 decimals, > 9 integer digits, non-digits.
 * Returns null when invalid.
 */
export function parsePriceTicks(input: string): number | null {
  const s = input.trim();
  if (!/^\d{1,9}(\.\d{1,2})?$/.test(s)) return null;
  const dot = s.indexOf('.');
  const whole = dot === -1 ? s : s.slice(0, dot);
  const frac = dot === -1 ? '' : s.slice(dot + 1);
  const ticks = Number(whole) * 100 + Number(frac.padEnd(2, '0'));
  return Number.isSafeInteger(ticks) ? ticks : null;
}

/** "10" -> 10. Rejects non-integers / negatives / > 5 digits. */
export function parseQty(input: string): number | null {
  const s = input.trim();
  if (!/^\d{1,5}$/.test(s)) return null;
  return Number(s);
}

/** Unsigned integer (u32-ish); rejects sign/fraction/junk. */
export function parseUInt(input: string, maxDigits = 10): number | null {
  const s = input.trim();
  if (!/^\d{1,15}$/.test(s) || s.length > maxDigits) return null;
  const n = Number(s);
  return Number.isSafeInteger(n) ? n : null;
}

/** Signed integer (i64-in-practice range clamped to i53). */
export function parseIntSafe(input: string): number | null {
  const s = input.trim().replace(/^\+/, '');
  if (!/^-?\d{1,15}$/.test(s)) return null;
  const n = Number(s);
  return Number.isSafeInteger(n) ? n : null;
}

// ---------------------------------------------------------------------------
// BigInt-safe integer math (never float-multiply money)
// ---------------------------------------------------------------------------

/** a * b in exact integer arithmetic; null on i53 overflow. */
export function mulInt(a: number, b: number): number | null {
  if (!Number.isInteger(a) || !Number.isInteger(b)) return null;
  const r = BigInt(a) * BigInt(b);
  if (r > BigInt(Number.MAX_SAFE_INTEGER) || r < BigInt(Number.MIN_SAFE_INTEGER)) return null;
  return Number(r);
}

/** a * b / 100 exact (e.g. percent bands); null on overflow. */
export function mulDiv100(a: number, b: number): number | null {
  if (!Number.isInteger(a) || !Number.isInteger(b)) return null;
  const r = (BigInt(a) * BigInt(b)) / 100n;
  if (r > BigInt(Number.MAX_SAFE_INTEGER) || r < BigInt(Number.MIN_SAFE_INTEGER)) return null;
  return Number(r);
}

/**
 * Strike ladder helper for /admin sessions: strikes around `markTicks`
 * spanning ±`pct`%, snapped to `step`-tick increments (default 500 ticks).
 * All integer arithmetic.
 */
export function generateStrikes(markTicks: number, pct = 3, step = 500): number[] {
  if (!Number.isInteger(markTicks) || markTicks <= 0 || step <= 0) return [];
  const lo = Math.ceil(markTicks * (100 - pct) / 100 / step) * step;
  const hi = Math.floor(markTicks * (100 + pct) / 100 / step) * step;
  const out: number[] = [];
  for (let s = lo; s <= hi && out.length < 512; s += step) out.push(s);
  return out;
}

/** Epoch ms -> value for <input type="datetime-local"> ("YYYY-MM-DDTHH:mm"). */
export function toLocalInputValue(ms: number): string {
  const d = new Date(ms);
  return `${d.getFullYear()}-${pad2(d.getMonth() + 1)}-${pad2(d.getDate())}T${pad2(d.getHours())}:${pad2(d.getMinutes())}`;
}
