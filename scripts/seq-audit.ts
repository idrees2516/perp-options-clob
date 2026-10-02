import { SimVenue, BookFrameSequencer } from "../packages/sim-engine/src/index.ts";
import type { BookUpdateTagged } from "../packages/types/src/transport.ts";

const venue = new SimVenue({ seed: 1234 });
venue.start();
venue.subscribe(["BTC-PERP"]);
const seq = new BookFrameSequencer();
let lastSeq = 0;
let frames = 0;
let gaps = 0;

function relabel(updates: Record<string, BookUpdateTagged>): Record<string, BookUpdateTagged> {
  const out: Record<string, BookUpdateTagged> = {};
  for (const [symbol, u] of Object.entries(updates)) {
    out[symbol] = u.kind === "snapshot" ? seq.snapshot(symbol, u.bids, u.asks) : seq.delta(symbol, u);
  }
  return out;
}

setTimeout(() => {
  // simulate subscribe → snapshot
  const snaps = relabel(venue.bookSnapshots(["BTC-PERP"]));
  lastSeq = snaps["BTC-PERP"]!.seq;
  const t = setInterval(() => {
    const out = venue.drain();
    for (const [sym, u] of Object.entries(relabel(out.books))) {
      frames++;
      if (u.seq !== lastSeq + 1) {
        gaps++;
        console.log(`GAP ${sym}: last=${lastSeq} frame=${u.seq}`);
      }
      lastSeq = u.seq;
    }
  }, 80);
  setTimeout(() => {
    clearInterval(t);
    console.log(`frames=${frames} gaps=${gaps} lastSeq=${lastSeq}`);
    process.exit(gaps === 0 ? 0 : 1);
  }, 8000);
}, 1000);
