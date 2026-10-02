import { SimVenue } from "../packages/sim-engine/src/index.ts";

const venue = new SimVenue({ seed: 4242 });
venue.start();
venue.setSpeed(60);
venue.subscribe(["BTC-PERP"]);

setTimeout(() => {
  console.log("t=0.1s mark:", venue.snapshot().markets.find((m) => m.symbol === "BTC-PERP")?.best_bid_ticks);
}, 100);

setTimeout(() => {
  console.log("t=1s mark:", venue.snapshot().markets.find((m) => m.symbol === "BTC-PERP")?.best_bid_ticks);
  // garbage command
  try {
    venue.command({ type: "place", now: 0, request: { subaccount: 7, garbage: true } as never });
    const out = venue.drain();
    console.log("garbage: no throw; events:", out.journal.map((e) => e.event.type));
  } catch (e) {
    console.log("garbage: THREW:", String(e).slice(0, 90));
  }
  process.exit(0);
}, 1000);
