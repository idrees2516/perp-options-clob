import { SimVenue } from "../packages/sim-engine/src/index.ts";
import type { Command } from "../packages/types/src/index.ts";

const venue = new SimVenue({ seed: 4242 });
venue.start();
venue.setSpeed(60);
venue.subscribe(["BTC-PERP"]);

setTimeout(() => {
  // Drain to see current state
  const out = venue.drain();
  const snap = venue.snapshot();
  const row = snap.markets.find((m) => m.symbol === "BTC-PERP");
  console.log("BTC-PERP mark/bid/ask:", row?.best_bid_ticks, row?.best_ask_ticks, "phase:", snap.meta.phase);
  console.log("initial journal:", out.journal.slice(0, 5).map((e) => e.type));

  const cmd: Command = {
    type: "place",
    now: 0,
    request: {
      subaccount: 7,
      symbol: "BTC-PERP",
      side: "bid",
      order_type: { kind: "market" },
      price_ticks: null,
      qty_lots: 1,
      tif: { kind: "ioc" },
      post_only: false,
      reduce_only: false,
      stp: "cancel_newest",
      display_lots: null,
      oco_group: null,
      client_ts: 0,
    },
  };
  try {
    venue.command(cmd);
    const after = venue.drain();
    console.log("after events:", after.journal.map((e) => e.type));
    console.log("after trades:", after.prints.length);
  } catch (e) {
    console.log("THREW:", String(e));
  }
  process.exit(0);
}, 2000);
