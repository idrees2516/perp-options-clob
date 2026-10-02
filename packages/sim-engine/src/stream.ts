/**
 * Book frame sequencing — the supplier half of the G-27 contract.
 *
 * The engine's `Book.seq` counts mutations; a drain tick batches many
 * mutations into one net delta, so raw engine seqs jump and would trip the
 * client's `seq === last + 1` gap rule on every frame. The wire contract is
 * about FRAMES, not mutations: every frame a consumer receives carries the
 * next seq of ITS stream, so a missing frame (real transport drop, reconnect
 * boundary, overflow) is detectable and demands a snapshot resync.
 *
 * One sequencer per consumer connection. Snapshots consume a seq too — they
 * reset the client's session baseline. Deltas relay the net level changes
 * with the next seq. Frames within a connection are FIFO, so per-connection
 * numbering is gap-free by construction; any gap you can see is real.
 */

import type { BookUpdateTagged, Level } from "@perp/types";

export class BookFrameSequencer {
  private counters = new Map<string, number>();

  /** Snapshot frame at the next stream seq (state is whatever the book is now). */
  snapshot(symbol: string, bids: Level[], asks: Level[]): BookUpdateTagged {
    const seq = this.next(symbol);
    return { kind: "snapshot", seq, bids, asks };
  }

  /** Relabel a raw delta frame with the next stream seq. */
  delta(symbol: string, frame: BookUpdateTagged): BookUpdateTagged {
    const seq = this.next(symbol);
    return { kind: "delta", seq, bids: frame.bids, asks: frame.asks };
  }

  /** Last seq emitted for a symbol (0 when nothing yet). */
  seen(symbol: string): number {
    return this.counters.get(symbol) ?? 0;
  }

  reset(symbol?: string): void {
    if (symbol === undefined) this.counters.clear();
    else this.counters.delete(symbol);
  }

  private next(symbol: string): number {
    const seq = (this.counters.get(symbol) ?? 0) + 1;
    this.counters.set(symbol, seq);
    return seq;
  }
}
