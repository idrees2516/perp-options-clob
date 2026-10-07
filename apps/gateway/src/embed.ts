/**
 * Embedded gateway boot — used by the Next.js `instrumentation.ts` hook.
 *
 * The web app's dev/prod server is the one process the platform guarantees to
 * keep alive, so the venue gateway runs *in-process* next to it:
 *   • no separate process supervision (watchdogs, nohup, setsid) needed
 *   • gateway lives exactly as long as the web server — clean lifecycle
 *   • in dev, Next restarts on instrumentation/config changes; the boot is
 *     idempotent (an already-serving gateway on the port is detected and
 *     reused) and EADDRINUSE is retried while a dying predecessor drains
 *
 * The standalone entrypoint (`src/index.ts` → `run()`) is unchanged and knows
 * nothing about this file.
 */

import { createGateway, type Gateway } from "./server";
import { log } from "./log";

/** Default embedded port — must match the web app's XTransformPort default. */
export const EMBED_PORT = 3031;

let booted: Promise<Gateway | null> | null = null;

function probe(url: string): Promise<boolean> {
  return fetch(url, { signal: AbortSignal.timeout(1_500) })
    .then((r) => r.ok)
    .catch(() => false);
}

function listen(
  server: Gateway["server"],
  port: number,
  host: string,
): Promise<void> {
  return new Promise((resolve, reject) => {
    const onError = (err: NodeJS.ErrnoException) => {
      server.removeListener("error", onError);
      reject(err);
    };
    server.once("error", onError);
    server.listen(port, host, () => {
      server.removeListener("error", onError);
      resolve();
    });
  });
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

/**
 * Boot the venue gateway in-process. Safe to call repeatedly:
 *  - concurrent calls share one boot
 *  - if some gateway already serves `port`, returns null (no second venue)
 *  - EADDRINUSE with no healthy listener is retried while a predecessor
 *    process drains (dev-server restart race)
 */
export function bootEmbeddedGateway(overrides?: {
  port?: number;
  host?: string;
  env?: Partial<Record<string, string>>;
}): Promise<Gateway | null> {
  if (booted) return booted;

  const port = overrides?.port ?? EMBED_PORT;
  const host = overrides?.host ?? "::"; // dual-stack: edge proxy may dial v4 or v6
  const health = `http://127.0.0.1:${port}/healthz`;

  booted = (async () => {
    // Already serving (in-process reload or a co-resident gateway)?
    if (await probe(health)) {
      log.info("gateway.embed.skip", { reason: "already-serving", port });
      return null;
    }

    const gateway = createGateway({
      GATEWAY_HOST: host,
      ...(overrides?.env ?? {}),
    });
    gateway.host.start();

    // A dying predecessor may hold the port for a moment after a dev-server
    // restart — retry while the port is busy but nothing answers on it.
    const RETRIES = 30;
    for (let attempt = 1; ; attempt++) {
      try {
        await listen(gateway.server, port, host);
        break;
      } catch (err) {
        const code = (err as NodeJS.ErrnoException).code;
        const busy = code === "EADDRINUSE" || code === "EACCES";
        if (busy && attempt < RETRIES) {
          if (await probe(health)) {
            // Someone else owns the port and is healthy — stand down.
            log.info("gateway.embed.skip", { reason: "took-over-during-boot", port });
            gateway.host.stop();
            return null;
          }
          log.warn("gateway.embed.retry", { attempt, code, port });
          await sleep(1_000);
          continue;
        }
        log.error("gateway.embed.failed", { error: String(err), port });
        gateway.host.stop();
        throw err;
      }
    }

    log.info("gateway.embed.listening", {
      host,
      port,
      note: "in-process with the Next.js server",
    });
    return gateway;
  })().catch((err) => {
    // Let future callers try again after a hard failure.
    booted = null;
    throw err;
  });

  return booted;
}
