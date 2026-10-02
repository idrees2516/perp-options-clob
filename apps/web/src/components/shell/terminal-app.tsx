"use client";

/**
 * The terminal application shell — a single-route trading terminal.
 * Left rail navigation, top market bar, view container, status strip.
 */

import { useEffect, Suspense, lazy } from "react";
import { connectVenue, useVenueStore } from "@/lib/venue-store";
import { LeftRail } from "./left-rail";
import { TopBar } from "./top-bar";
import { StatusBar } from "./status-bar";
import { ConnectionDialog } from "./connection-dialog";
import { TerminalView } from "@/views/terminal/terminal-view";
import { MarketsSkeleton } from "./skeletons";

const MarketsView = lazy(() => import("@/views/markets/markets-view").then((m) => ({ default: m.MarketsView })));
const PortfolioView = lazy(() => import("@/views/portfolio/portfolio-view").then((m) => ({ default: m.PortfolioView })));
const FundingView = lazy(() => import("@/views/funding/funding-view").then((m) => ({ default: m.FundingView })));
const RiskView = lazy(() => import("@/views/risk/risk-view").then((m) => ({ default: m.RiskView })));
const IncentivesView = lazy(() => import("@/views/incentives/incentives-view").then((m) => ({ default: m.IncentivesView })));
const VaultsView = lazy(() => import("@/views/vaults/vaults-view").then((m) => ({ default: m.VaultsView })));
const RfqView = lazy(() => import("@/views/rfq/rfq-view").then((m) => ({ default: m.RfqView })));
const GovernanceView = lazy(() => import("@/views/governance/governance-view").then((m) => ({ default: m.GovernanceView })));
const ReservesView = lazy(() => import("@/views/reserves/reserves-view").then((m) => ({ default: m.ReservesView })));
const JournalView = lazy(() => import("@/views/journal/journal-view").then((m) => ({ default: m.JournalView })));
const ConsoleView = lazy(() => import("@/views/console/console-view").then((m) => ({ default: m.ConsoleView })));

export function TerminalApp() {
  const view = useVenueStore((s) => s.view);
  const connected = useVenueStore((s) => s.connected);

  useEffect(() => {
    connectVenue();
  }, []);

  return (
    <div className="h-dvh w-full flex flex-col venue-bg overflow-hidden">
      <TopBar />
      <div className="flex flex-1 min-h-0">
        <LeftRail />
        <main className="flex-1 min-w-0 min-h-0 overflow-hidden" aria-label="Venue workspace">
          {connected ? (
            <Suspense fallback={<MarketsSkeleton />}>
              <ViewRouter view={view} />
            </Suspense>
          ) : (
            <BootScreen />
          )}
        </main>
      </div>
      <StatusBar />
      <ConnectionDialog />
    </div>
  );
}

function ViewRouter({ view }: { view: string }) {
  switch (view) {
    case "terminal":
      return <TerminalView />;
    case "markets":
      return <MarketsView />;
    case "portfolio":
      return <PortfolioView />;
    case "funding":
      return <FundingView />;
    case "risk":
      return <RiskView />;
    case "incentives":
      return <IncentivesView />;
    case "vaults":
      return <VaultsView />;
    case "rfq":
      return <RfqView />;
    case "governance":
      return <GovernanceView />;
    case "reserves":
      return <ReservesView />;
    case "journal":
      return <JournalView />;
    case "console":
      return <ConsoleView />;
    default:
      return <TerminalView />;
  }
}

function BootScreen() {
  return (
    <div className="h-full w-full flex items-center justify-center">
      <div className="flex flex-col items-center gap-5">
        <div className="relative">
          <div className="w-16 h-16 rounded-2xl panel panel-glow flex items-center justify-center">
            <span className="font-mono text-2xl font-bold text-primary">POC</span>
          </div>
          <span className="absolute -right-1 -top-1 w-3.5 h-3.5 rounded-full bg-up pulse-dot" />
        </div>
        <div className="text-center space-y-1.5">
          <p className="text-sm font-medium">Booting deterministic venue…</p>
          <p className="text-xs text-muted-foreground">
            Seeding the engine · listing the chain · connecting market data
          </p>
        </div>
        <div className="w-48 h-1 rounded-full overflow-hidden bg-muted">
          <div className="h-full w-1/3 shimmer rounded-full" />
        </div>
      </div>
    </div>
  );
}
