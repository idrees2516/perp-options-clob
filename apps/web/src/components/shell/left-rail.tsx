"use client";

/**
 * Left rail — icon navigation between the venue's surfaces.
 * Keyboard numbers 1-9 switch views (terminal muscle memory).
 */

import { useEffect } from "react";
import { useVenueStore, type ViewId } from "@/lib/venue-store";
import {
  CandlestickChart,
  LayoutGrid,
  Wallet,
  Waves,
  ShieldAlert,
  Trophy,
  Vault,
  MessagesSquare,
  Scale,
  FileLock2,
  ScrollText,
  TerminalSquare,
} from "lucide-react";
import {
  Tooltip,
  TooltipContent,
  TooltipProvider,
  TooltipTrigger,
} from "@/components/ui/tooltip";

interface NavItem {
  id: ViewId;
  label: string;
  hint: string;
  icon: React.ComponentType<{ className?: string }>;
  hotkey: string;
}

const NAV: NavItem[] = [
  { id: "terminal", label: "Terminal", hint: "Trade — book, tape, orders", icon: CandlestickChart, hotkey: "1" },
  { id: "markets", label: "Markets", hint: "Chain, marks, IV surface", icon: LayoutGrid, hotkey: "2" },
  { id: "portfolio", label: "Portfolio", hint: "Positions, margin, collateral", icon: Wallet, hotkey: "3" },
  { id: "funding", label: "Funding", hint: "Premium + interest, rolls", icon: Waves, hotkey: "4" },
  { id: "risk", label: "Risk", hint: "Liquidations, insurance, breakers", icon: ShieldAlert, hotkey: "5" },
  { id: "incentives", label: "Incentives", hint: "MM tiers, rewards, revenue", icon: Trophy, hotkey: "6" },
  { id: "vaults", label: "Vaults", hint: "LP underwriters, epochs, NAV", icon: Vault, hotkey: "7" },
  { id: "rfq", label: "RFQ", hint: "Multi-leg dealer liquidity", icon: MessagesSquare, hotkey: "8" },
  { id: "governance", label: "Governance", hint: "Multisig, timelock, veto", icon: Scale, hotkey: "9" },
  { id: "reserves", label: "Reserves", hint: "Merkle proof-of-reserves", icon: FileLock2, hotkey: "0" },
  { id: "journal", label: "Journal", hint: "The event stream itself", icon: ScrollText, hotkey: "j" },
  { id: "console", label: "Console", hint: "Raw command surface", icon: TerminalSquare, hotkey: "k" },
];

export function LeftRail() {
  const view = useVenueStore((s) => s.view);
  const setView = useVenueStore((s) => s.setView);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.target instanceof HTMLInputElement || e.target instanceof HTMLTextAreaElement || e.target instanceof HTMLSelectElement) return;
      if (e.metaKey || e.ctrlKey || e.altKey) return;
      const key = e.key.toLowerCase();
      const item = NAV.find((n) => n.hotkey === key);
      if (item) {
        e.preventDefault();
        setView(item.id);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [setView]);

  return (
    <TooltipProvider delayDuration={150}>
      <nav
        aria-label="Venue navigation"
        className="w-[52px] shrink-0 border-r border-hairline bg-card/40 backdrop-blur-sm flex flex-col items-center py-2 gap-0.5 z-20"
      >
        {NAV.map((item) => {
          const active = view === item.id;
          const Icon = item.icon;
          return (
            <Tooltip key={item.id}>
              <TooltipTrigger asChild>
                <button
                  onClick={() => setView(item.id)}
                  aria-current={active ? "page" : undefined}
                  aria-label={item.label}
                  className={`group relative w-9 h-9 rounded-lg flex items-center justify-center transition-all duration-150 focus-glow ${
                    active
                      ? "bg-primary/15 text-primary"
                      : "text-muted-foreground hover:text-foreground hover:bg-muted/70"
                  }`}
                >
                  <Icon className="w-[17px] h-[17px]" />
                  {active && (
                    <span className="absolute left-[-8px] top-1/2 -translate-y-1/2 w-[3px] h-5 rounded-full bg-primary" />
                  )}
                  <span className="absolute right-1 bottom-0 text-[8px] font-mono opacity-0 group-hover:opacity-60 text-muted-foreground">
                    {item.hotkey}
                  </span>
                </button>
              </TooltipTrigger>
              <TooltipContent side="right" sideOffset={6} className="px-2.5 py-1.5">
                <p className="text-xs font-medium">{item.label}</p>
                <p className="text-[10px] text-muted-foreground">{item.hint}</p>
              </TooltipContent>
            </Tooltip>
          );
        })}
      </nav>
    </TooltipProvider>
  );
}
