"use client";

/**
 * Top bar — identity, live market ticker strip, sim clock + speed control,
 * account switcher, theme. The venue's pulse is visible from every view.
 */

import { memo, useEffect, useState } from "react";
import { useTheme } from "next-themes";
import { useVenueStore, marketRowFor, liveAccount } from "@/lib/venue-store";
import { getVenueClient } from "@perp/api-client";
import { usd, changePct, pct, simTime } from "@/lib/fmt";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Pause, Play, Gauge, Moon, Sun, RefreshCw, RotateCcw } from "lucide-react";
import { toast } from "sonner";

const SPEEDS = [
  { label: "1×", value: 1 },
  { label: "60×", value: 60 },
  { label: "600×", value: 600 },
  { label: "3600×", value: 3600 },
];

const PHASE_LABELS: Record<string, string> = {
  genesis: "Genesis",
  "market-making": "Market making",
  "taker-flow": "Taker flow",
  "oracle-defense": "Oracle defense",
  shock: "Liquidation shock",
  recovery: "Recovery",
  "long-run": "Long run",
};

export const TopBar = memo(function TopBar() {
  const snapshot = useVenueStore((s) => s.snapshot);
  const markets = useVenueStore((s) => s.markets);
  const activeAccount = useVenueStore((s) => s.activeAccount);
  const setActiveAccount = useVenueStore((s) => s.setActiveAccount);
  const speed = useVenueStore((s) => s.speed);
  const setSpeed = useVenueStore((s) => s.setSpeed);
  const resetVenue = useVenueStore((s) => s.resetVenue);
  const refresh = useVenueStore((s) => s.refresh);
  const [clock, setClock] = useState<number>(0);
  const { theme, setTheme } = useTheme();

  // 1 Hz clock tick (for the sim clock display)
  useEffect(() => {
    const t = setInterval(() => setClock(Date.now()), 1000);
    return () => clearInterval(t);
  }, []);

  const meta = snapshot?.meta;
  const paused = speed === 0;
  const account = liveAccount(snapshot, useVenueStore.getState().accountDeltas, activeAccount);
  const descriptor = snapshot?.accountDescriptors.find((d) => d.id === activeAccount);
  const spotRow = marketRowFor(markets, snapshot, "BTC-PERP");
  const spot = spotRow?.mark_quote_minor ?? null;
  const spotPct = spotRow ? changePct(spotRow) : null;

  return (
    <header className="h-12 shrink-0 border-b border-hairline bg-card/60 backdrop-blur-md flex items-center gap-2 px-3 z-30">
      {/* Brand */}
      <div className="flex items-center gap-2 pr-2">
        <div className="w-7 h-7 rounded-lg bg-primary/15 border border-primary/25 flex items-center justify-center">
          <span className="font-mono text-[10px] font-bold text-primary">POC</span>
        </div>
        <div className="hidden md:block leading-none">
          <p className="text-[13px] font-semibold tracking-tight">perp-options-clob</p>
          <p className="text-[9.5px] text-muted-foreground font-mono">event-sourced venue</p>
        </div>
      </div>

      <div className="w-px h-6 bg-hairline hidden md:block" />

      {/* Spot + oracle pulse */}
      <div className="flex items-center gap-2 min-w-0">
        <span className={`w-1.5 h-1.5 rounded-full ${paused ? "bg-amber-400" : "bg-up pulse-dot"}`} />
        <span className="text-xs text-muted-foreground hidden sm:block">BTC/USD</span>
        <span className="num text-[13px] font-semibold">{usd(spot)}</span>
        {spotPct != null && (
          <span className={`num text-[11px] ${spotPct >= 0 ? "text-up" : "text-down"}`}>{pct(spotPct)}</span>
        )}
      </div>

      {/* Phase chip */}
      {meta?.phase && (
        <div className="hidden lg:flex items-center gap-1.5 px-2 py-0.5 rounded-md bg-muted/60 border border-hairline">
          <span className="w-1 h-1 rounded-full bg-chart-3" />
          <span className="text-[10px] text-muted-foreground tracking-wide uppercase">
            {PHASE_LABELS[meta.phase] ?? meta.phase}
          </span>
        </div>
      )}

      <div className="flex-1 min-w-0" />

      {/* Sim clock + speed */}
      <div className="flex items-center gap-1.5 mr-1">
        <span className="num text-[11px] text-muted-foreground hidden xl:block tabular-nums" title="Venue engine clock">
          {simTime(meta?.now)} <span className="opacity-50">UTC</span>
        </span>
        <button
          onClick={() => setSpeed(paused ? 600 : 0)}
          className="w-7 h-7 rounded-md flex items-center justify-center text-muted-foreground hover:text-foreground hover:bg-muted/70 transition-colors focus-glow"
          aria-label={paused ? "Resume venue clock" : "Pause venue clock"}
          title={paused ? "Resume (p)" : "Pause (p)"}
        >
          {paused ? <Play className="w-3.5 h-3.5" /> : <Pause className="w-3.5 h-3.5" />}
        </button>
        <Select value={String(speed)} onValueChange={(v) => setSpeed(Number(v))}>
          <SelectTrigger
            size="sm"
            className="h-7 w-[72px] text-[11px] num border-hairline bg-transparent gap-1"
            aria-label="Simulation speed"
          >
            <Gauge className="w-3 h-3 opacity-60" />
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            {SPEEDS.map((s) => (
              <SelectItem key={s.value} value={String(s.value)} className="text-xs">
                {s.label}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      </div>

      <div className="w-px h-6 bg-hairline" />

      {/* Account switcher */}
      <DropdownMenu>
        <DropdownMenuTrigger className="flex items-center gap-2 h-7 px-2 rounded-md hover:bg-muted/70 transition-colors focus-glass focus-glow outline-none">
          <span
            className="w-2 h-2 rounded-full shrink-0"
            style={{ background: descriptor?.color ?? "#888" }}
          />
          <span className="text-xs font-medium hidden sm:block">{descriptor?.name ?? `Sub ${activeAccount}`}</span>
          <span className="num text-[11px] text-muted-foreground hidden lg:block">
            {account ? usd(account.equity_quote_minor) : "—"}
          </span>
        </DropdownMenuTrigger>
        <DropdownMenuContent align="end" className="w-64">
          <DropdownMenuLabel className="text-[10px] uppercase tracking-wider text-muted-foreground">
            Subaccounts — margin pooled per account
          </DropdownMenuLabel>
          {(snapshot?.accountDescriptors ?? []).map((d) => {
            const a = liveAccount(snapshot, useVenueStore.getState().accountDeltas, d.id);
            return (
              <DropdownMenuItem
                key={d.id}
                onClick={() => setActiveAccount(d.id)}
                className={`gap-2 py-1.5 ${d.id === activeAccount ? "bg-primary/10" : ""}`}
              >
                <span className="w-2 h-2 rounded-full shrink-0" style={{ background: d.color }} />
                <div className="flex-1 min-w-0">
                  <p className="text-xs font-medium">{d.name}</p>
                  <p className="text-[10px] text-muted-foreground">{d.role}</p>
                </div>
                <span className="num text-[11px] text-muted-foreground">
                  {a ? usd(a.equity_quote_minor) : "—"}
                </span>
              </DropdownMenuItem>
            );
          })}
          <DropdownMenuSeparator />
          <DropdownMenuItem
            onClick={() => {
              getVenueClient().send({ type: "command", command: { type: "deposit", subaccount: activeAccount, amount_quote_minor: 100_000_00n } });
              toast.success(`Faucet: deposited $100,000.00 to ${descriptor?.name}`, {
                description: "Command::Deposit → Event::Deposit",
              });
            }}
            className="gap-2"
          >
            <span className="w-2 h-2 rounded-full bg-up" />
            <span className="text-xs">Deposit $100k (faucet)</span>
          </DropdownMenuItem>
        </DropdownMenuContent>
      </DropdownMenu>

      {/* Controls */}
      <div className="flex items-center">
        <button
          onClick={() => refresh()}
          className="w-7 h-7 rounded-md flex items-center justify-center text-muted-foreground hover:text-foreground hover:bg-muted/70 transition-colors focus-glow"
          aria-label="Refresh snapshot"
          title="Request full snapshot"
        >
          <RefreshCw className="w-3.5 h-3.5" />
        </button>
        <button
          onClick={() => {
            resetVenue(Math.floor(Math.random() * 1_000_000));
            toast("Venue reset", { description: "Fresh genesis with a new seed" });
          }}
          className="w-7 h-7 rounded-md flex items-center justify-center text-muted-foreground hover:text-foreground hover:bg-muted/70 transition-colors focus-glow"
          aria-label="Reset venue"
          title="Reset venue (new seed)"
        >
          <RotateCcw className="w-3.5 h-3.5" />
        </button>
        <button
          onClick={() => setTheme(theme === "dark" ? "light" : "dark")}
          className="w-7 h-7 rounded-md flex items-center justify-center text-muted-foreground hover:text-foreground hover:bg-muted/70 transition-colors focus-glow"
          aria-label="Toggle theme"
          title="Toggle theme"
        >
          <Sun className="w-3.5 h-3.5 dark:hidden" />
          <Moon className="w-3.5 h-3.5 hidden dark:block" />
        </button>
      </div>
      <span className="sr-only" aria-live="polite">
        Venue clock {simTime(meta?.now)}, speed {speed}×, phase {meta?.phase}
      </span>
      <span className="hidden">{clock}</span>
    </header>
  );
});
