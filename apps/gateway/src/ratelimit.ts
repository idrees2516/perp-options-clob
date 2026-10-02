/**
 * Token bucket rate limiting (G-25 client contract: drain → 429 → back off).
 * One bucket per key / per socket / per IP — lazily created, swept on access.
 */

export class TokenBucket {
  private tokens: number;
  private lastRefill: number;

  constructor(
    readonly capacity: number,
    readonly refillPerSec: number,
  ) {
    this.tokens = capacity;
    this.lastRefill = Date.now();
  }

  /** Try to consume `n` tokens. */
  tryRemove(n = 1): boolean {
    this.refill();
    if (this.tokens < n) return false;
    this.tokens -= n;
    return true;
  }

  /** Milliseconds until `n` tokens would be available (for Retry-After). */
  retryAfterMs(n = 1): number {
    this.refill();
    if (this.tokens >= n) return 0;
    const missing = n - this.tokens;
    return Math.ceil((missing / this.refillPerSec) * 1000);
  }

  /** Milliseconds since the bucket was last touched. */
  idleMs(): number {
    return Date.now() - this.lastRefill;
  }

  private refill(): void {
    const now = Date.now();
    const elapsedSec = (now - this.lastRefill) / 1000;
    if (elapsedSec <= 0) return;
    this.tokens = Math.min(this.capacity, this.tokens + elapsedSec * this.refillPerSec);
    this.lastRefill = now;
  }
}

/** Bucket registry keyed by string (api key id, socket id, ip). */
export class BucketRegistry {
  private buckets = new Map<string, TokenBucket>();

  constructor(
    private capacity: number,
    private refillPerSec: number,
    private maxTracked = 10_000,
  ) {}

  of(key: string): TokenBucket {
    let b = this.buckets.get(key);
    if (!b) {
      if (this.buckets.size >= this.maxTracked) {
        // Sweep: drop idle buckets (cheap and effective under abuse).
        const cutoff = Date.now() - 60_000;
        for (const [k, v] of this.buckets) {
          if (v.idleMs() > 60_000) this.buckets.delete(k);
        }
      }
      b = new TokenBucket(this.capacity, this.refillPerSec);
      this.buckets.set(key, b);
    }
    return b;
  }

  delete(key: string): void {
    this.buckets.delete(key);
  }
}
