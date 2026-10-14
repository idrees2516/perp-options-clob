'use client';

/**
 * /trade — the terminal.
 *
 * Desktop-first professional density: topbar + offline banner + underlying
 * strip, then a 3-column grid — chain (left, ~340px), depth + tape (center),
 * order ticket + open orders (right) — and a full-width bottom strip with
 * positions + balances. Every panel sits behind its own error boundary.
 * On small screens the columns stack and the page scrolls.
 */
import { RequireSession } from '@/components/RequireSession';
import { TerminalProvider } from '@/providers/TerminalProvider';
import { TopBar } from '@/components/topbar/TopBar';
import { OfflineBanner } from '@/components/topbar/OfflineBanner';
import { UnderlyingBar } from '@/components/chain/UnderlyingBar';
import { ChainTable } from '@/components/chain/ChainTable';
import { PanelErrorBoundary } from '@/components/ErrorBoundary';
import { DepthLadder } from '@/components/trade/DepthLadder';
import { Tape } from '@/components/trade/Tape';
import { OrderTicket } from '@/components/trade/OrderTicket';
import { OpenOrders } from '@/components/trade/OpenOrders';
import { PositionsPanel } from '@/components/trade/PositionsPanel';

export default function TradePage() {
  return (
    <RequireSession>
      <TerminalProvider>
        <div className="flex h-dvh flex-col overflow-hidden bg-bg text-txt">
          <TopBar />
          <OfflineBanner />
          <div className="flex min-h-0 flex-1 flex-col gap-2 overflow-y-auto p-2 lg:overflow-hidden">
            <UnderlyingBar />
            <main
              className="grid min-h-0 flex-1 grid-cols-1 gap-2 lg:grid-cols-[minmax(280px,340px)_minmax(0,1fr)_minmax(280px,360px)] lg:grid-rows-[minmax(0,1fr)_230px]"
              aria-label="Trading terminal"
            >
              {/* left — option chain */}
              <div className="h-[460px] min-h-0 lg:h-auto">
                <PanelErrorBoundary title="Chain">
                  <ChainTable />
                </PanelErrorBoundary>
              </div>

              {/* center — depth + tape */}
              <div className="flex min-h-0 flex-col gap-2">
                <div className="h-[340px] min-h-0 shrink-0 lg:h-auto lg:min-h-0 lg:flex-1 lg:shrink">
                  <PanelErrorBoundary title="Depth">
                    <DepthLadder />
                  </PanelErrorBoundary>
                </div>
                <div className="h-[260px] min-h-0 shrink-0 lg:h-auto lg:min-h-0 lg:flex-1 lg:shrink">
                  <PanelErrorBoundary title="Tape">
                    <Tape />
                  </PanelErrorBoundary>
                </div>
              </div>

              {/* right — ticket + working orders */}
              <div className="flex min-h-0 flex-col gap-2 lg:overflow-y-auto">
                <div className="min-h-0 shrink-0">
                  <PanelErrorBoundary title="Order ticket">
                    <OrderTicket />
                  </PanelErrorBoundary>
                </div>
                <div className="h-[300px] min-h-0 shrink-0 lg:h-[320px]">
                  <PanelErrorBoundary title="Open orders">
                    <OpenOrders />
                  </PanelErrorBoundary>
                </div>
              </div>

              {/* bottom strip — positions + balances */}
              <div className="h-[240px] min-h-0 lg:col-span-3 lg:h-auto">
                <PanelErrorBoundary title="Positions">
                  <PositionsPanel />
                </PanelErrorBoundary>
              </div>
            </main>
          </div>
        </div>
      </TerminalProvider>
    </RequireSession>
  );
}
