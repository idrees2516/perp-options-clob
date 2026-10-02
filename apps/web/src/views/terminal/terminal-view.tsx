"use client";

/**
 * The Terminal — the trading core.
 *
 * ┌──────────────┬────────────┬──────────┐
 * │ Chart + Tape │  Book      │  Ticket  │
 * ├──────────────┴────────────┴──────────┤
 * │ Positions / Orders / Fills / Greeks  │
 * └──────────────────────────────────────┘
 */

import { Suspense, lazy, memo } from "react";
import { Panel, PanelGroup, PanelResizeHandle } from "react-resizable-panels";
import { BookLadder } from "./book-ladder";
import { PriceChart } from "./price-chart";
import { TradeTape } from "./trade-tape";
import { OrderEntry } from "./order-entry";
import { MarketHeader } from "./market-header";
import { BottomTabs } from "./bottom-tabs";

const AccountStrip = lazy(() => import("./account-strip").then((m) => ({ default: m.AccountStrip })));

export function TerminalView() {
  return (
    <div className="h-full w-full flex flex-col">
      <MarketHeader />
      <div className="flex-1 min-h-0 flex flex-col">
        <PanelGroup direction="vertical" className="flex-1 min-h-0">
          <Panel defaultSize={62} minSize={30}>
            <PanelGroup direction="horizontal">
              <Panel defaultSize={54} minSize={25}>
                <div className="h-full w-full flex flex-col">
                  <div className="flex-1 min-h-0 border-b border-hairline bg-panel/30">
                    <PriceChart />
                  </div>
                  <div className="h-[136px] shrink-0 bg-panel/10">
                    <TradeTape />
                  </div>
                </div>
              </Panel>
              <Handle />
              <Panel defaultSize={24} minSize={16}>
                <BookLadder />
              </Panel>
              <Handle />
              <Panel defaultSize={22} minSize={20}>
                <OrderEntry />
              </Panel>
            </PanelGroup>
          </Panel>
          <Handle />
          <Panel defaultSize={38} minSize={15}>
            <div className="h-full flex flex-col">
              <Suspense fallback={<div className="h-14 shimmer border-b border-hairline" />}>
                <AccountStrip />
              </Suspense>
              <div className="flex-1 min-h-0">
                <BottomTabs />
              </div>
            </div>
          </Panel>
        </PanelGroup>
      </div>
    </div>
  );
}

const Handle = memo(function Handle() {
  return (
    <PanelResizeHandle className="w-[3px] flex items-center justify-center group cursor-col-resize data-[orientation=vertical]:h-[3px] data-[orientation=vertical]:w-full data-[orientation=vertical]:cursor-row-resize">
      <div className="w-[3px] h-10 rounded-full bg-transparent group-hover:bg-primary/50 transition-colors data-[orientation=vertical]:w-10 data-[orientation=vertical]:h-[3px]" />
    </PanelResizeHandle>
  );
});
