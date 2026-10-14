'use client';

/**
 * /admin — the venue console (admin-scope keys only, gated by RequireAdmin).
 * Reuses the terminal wiring (TopBar, OfflineBanner, TerminalProvider) so the
 * cards get the signed VenueClient and live underlying marks; cards themselves
 * are independent panels behind error boundaries.
 */
import { RequireAdmin, RequireSession } from '@/components/RequireSession';
import { TerminalProvider } from '@/providers/TerminalProvider';
import { TopBar } from '@/components/topbar/TopBar';
import { OfflineBanner } from '@/components/topbar/OfflineBanner';
import { PanelErrorBoundary } from '@/components/ErrorBoundary';
import { FundingCard } from '@/components/admin/FundingCard';
import { SessionsCard } from '@/components/admin/SessionsCard';
import { MarketOpsCard } from '@/components/admin/MarketOpsCard';
import { VenueHealthCard } from '@/components/admin/VenueHealthCard';
import { KeysCard } from '@/components/admin/KeysCard';

export default function AdminPage() {
  return (
    <RequireSession>
      <RequireAdmin>
        <TerminalProvider>
          <div className="flex h-dvh flex-col overflow-hidden bg-bg text-txt">
            <TopBar />
            <OfflineBanner />
            <main className="min-h-0 flex-1 overflow-y-auto p-3" aria-label="Venue admin console">
              <div className="grid grid-cols-1 gap-3 xl:grid-cols-2">
                <PanelErrorBoundary title="Funding">
                  <FundingCard />
                </PanelErrorBoundary>
                <PanelErrorBoundary title="Sessions">
                  <SessionsCard />
                </PanelErrorBoundary>
                <PanelErrorBoundary title="Market ops">
                  <MarketOpsCard />
                </PanelErrorBoundary>
                <PanelErrorBoundary title="Keys">
                  <KeysCard />
                </PanelErrorBoundary>
                <div className="xl:col-span-2">
                  <PanelErrorBoundary title="Venue health">
                    <VenueHealthCard />
                  </PanelErrorBoundary>
                </div>
              </div>
            </main>
          </div>
        </TerminalProvider>
      </RequireAdmin>
    </RequireSession>
  );
}
