# Frontend contracts — perp-options-clob terminal

Read this fully before writing code. Everything you need is here or in the files it points to.

## What this app is

A single-route trading terminal (`/` renders `src/components/shell/terminal-app.tsx`) for a
perpetual + options CLOB protocol. A deterministic venue simulator runs in a Web Worker and
streams: market data (books as snapshot/delta with seq), trade prints, the event journal,
account deltas, market rows, and a full state snapshot every 2.5s. All money is bigint
quote minor units ($1.00 == 100n). All timestamps are ms since epoch.

Views switch client-side (no routing). The left rail (`components/shell/left-rail.tsx`)
switches `view` in the store. Your view file is lazy-loaded from
`components/shell/terminal-app.tsx` — the import line already exists, pointing at
`src/views/<name>/<name>-view.tsx` exporting a named component (check the exact name in
terminal-app.tsx).

## Transports — sim worker or live gateway (same contract)

The store's `mode` is `"sim"` (embedded Web Worker, zero backend) or `"remote"`
(`apps/gateway` — the production socket.io venue). Both speak the identical
`VenueControl` / `VenueMessage` contract; views never know which one is active.

- Switch transports: status bar `SIM`/`GATEWAY` chip or the cable icon (top bar) →
  connection dialog (`components/shell/connection-dialog.tsx`). Origin may carry a
  mount path (`https://host/gateway`); empty origin = same-origin `XTransformPort`
  proxy. Credentials (G-25 HMAC) are optional — demo gateways run auth-off.
- Remote-only store slices: `mode`, `gateway` (`{status, latencyMs, reconnects, error,
  label}`), `credentials`, `connectionOpen` + actions `connectSim()`,
  `connectRemote(endpoint?, creds?)`, `disconnectVenue()`, `setConnectionOpen(bool)`.
- `trade_fill` receipts arrive for commands your active account participated in —
  the store toasts them automatically.
- Over the network, frames are wire-encoded (bigint → `{"$bigint":"…"}` markers in
  `@perp/types`); transports encode/decode transparently.

## Store — `src/lib/venue-store.ts` (read it first)

```ts
import { useVenueStore } from "@/lib/venue-store";

const snapshot = useVenueStore((s) => s.snapshot);       // VenueSnapshot | null — refreshed every ~2.5s
const markets = useVenueStore((s) => s.markets);           // MarketRow[] live
const journal = useVenueStore((s) => s.journal);          // JournalEntry[] (last 600)
const prints = useVenueStore((s) => s.prints);             // TradePrintLike[] (last 160)
const books = useVenueStore((s) => s.books);               // Record<symbol, {bids, asks, seq, desynced}>
const account = useVenueStore((s) => s.activeAccount);    // number (default 7 = "You")
const activeSymbol = useVenueStore((s) => s.activeSymbol);
```

Types live in `@perp/types` (read `packages/types/src/venue.ts`, `protocol.ts`, `economics.ts`,
`event.ts` for exact shapes — `VenueSnapshot` is the master shape).

## Sending commands — `getVenueClient()`

```ts
import { getVenueClient } from "@perp/api-client";
const client = getVenueClient();

// Engine commands (same shapes as the Rust Command enum — packages/types/src/command.ts)
client.send({ type: "command", command: { type: "deposit", subaccount: 7, amount_quote_minor: 100_000_00n } });
client.send({ type: "command", command: { type: "transfer", from: 7, to: 3, amount_quote_minor: 10_000_00n, now: snapshot!.meta.now } });
client.send({ type: "command", command: { type: "vault_subscribe", vault_id: 1, subaccount: 7, amount_quote_minor: 5_000_00n, now: snapshot!.meta.now } });
client.send({ type: "command", command: { type: "exercise", subaccount: 7, symbol: "BTC-...-80000-C", lots: 5, now: snapshot!.meta.now } });

// Governance (separate surface)
const err = client.governance({ type: "propose", key: "fees.tier0.taker_bps", value: 3n, description: "...", proposer: "alice.ops" });
// actions: propose / approve / queue(auto) / execute / cancel / veto / sweep_expiries — see @perp/types GovernanceAction

// Withdrawal pipeline
client.send({ type: "withdrawal", op: "request", subaccount: 7, amount_quote_minor: 1_000_00n, destination: "0xf00d" });
client.send({ type: "withdrawal", op: "approve", id: 1 });      // operator approval
client.send({ type: "withdrawal", op: "cancel", id: 1 });

// Proof of reserves
client.send({ type: "por_build" });                              // async → next snapshot carries por.report + por.rows

// Oracle defense demo
client.send({ type: "oracle_inject", provider: "apis3", price_quote_minor: 160_000_00n });

// Full snapshot refresh
client.send({ type: "request_snapshot" });
```

Governance errors return a string (null on success). Toast feedback via `import { toast } from "sonner"`.

## Formatters — `src/lib/fmt.ts` (read it)

`usd(minor, {sign, compact})`, `usdCompact`, `pnl(minor)` (signed), `priceQuote(minor)`,
`ticksToPrice(ticks, inst)`, `sizeBase(lots, inst)`, `changePct(row)`, `pct(x)`, `bps(x)`,
`iv(x)` (0.55→"55.0%"), `simTime(ts)`, `simDate(ts)`, `relTime(ts, now)` ("in 3d"),
`shortSymbol(sym)` ("Mar 27 80,000C"), `healthClass(h)`, `sideClass(side)`, `healthLabel(h)`.
ALL numbers in tables/values must be wrapped in these (they apply the `num` mono class where
relevant — for custom spans use className "num" for numeric cells).

## Design language (MANDATORY — match the terminal view exactly)

- Dark-first quant terminal. Use the existing CSS variables via Tailwind classes:
  `bg-panel`, `bg-panel-2`, `border-hairline`, `text-muted-foreground`, `text-up` (green),
  `text-down` (red), `text-primary` (teal), `text-chart-3` (amber).
- Panels: `className="panel"` (or `panel-2`) — rounded, hairline border. Subtle headers:
  `text-[9px] uppercase tracking-wider text-muted-foreground`.
- Numbers: always `num` class (mono tabular). Right-align numeric table cells.
- Tables: follow `src/views/terminal/bottom-tabs.tsx` — local `Th`/`Td`/`ScrollTable`
  conventions (sticky header `bg-panel`, `text-[9px]` uppercase labels, rows `hover:bg-muted/30`,
  `border-b border-hairline/40`).
- Page scaffold every view uses:
  ```tsx
  <div className="h-full flex flex-col min-h-0">
    {/* toolbar: h-11 border-b border-hairline flex items-center gap-2 px-3 */}
    <div className="flex-1 min-h-0 overflow-auto scroll-thin p-3">…content…</div>
  </div>
  ```
- Stat tiles: `panel p-3` with label `text-[9px] uppercase tracking-wider text-muted-foreground`
  and value `num text-lg font-semibold`.
- Empty states: centered `text-muted-foreground/50 text-[11px]` with a short protocol-flavored
  line (e.g. "No proposals yet — the multisig is quiet").
- No blue/indigo. No emoji. Lucide icons only (`lucide-react`), sized 3.5–4 (14–16px).
- Feedback: `toast.success(...)` / `toast.error(...)` with `{ description: "Command::X → Event::Y" }`
  protocol citations in the description where natural.
- Responsive: content must not overflow at 1280px; tables scroll inside their containers.
- A11y: semantic elements, aria-labels on icon buttons, `sr-only` where helpful.

## Quality bar

- TypeScript strict — no `any`, no `@ts-ignore`.
- Exported component must be `memo()`-wrapped, named exactly as imported in terminal-app.tsx.
- Subscribe ONLY to store slices you render (selector-per-value, not the whole store object).
- `bun run lint` from repo root must show ZERO new errors for your files (warnings ok if justified).
- Do not modify: store, worker, shell components, other views, packages/*. Only your view files.
  Shared local components go in your view's folder.
