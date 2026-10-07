/**
 * Next.js instrumentation — boots the venue gateway in-process.
 *
 * The sandbox/production topology guarantees exactly one long-lived process:
 * the Next.js server. Running the gateway inside it means the "Live gateway"
 * transport is always available whenever the terminal itself is, with no
 * external supervision (watchdog scripts, detached processes) to drift.
 *
 * Wire-up:
 *   browser  →  same-origin XTransformPort=3031  →  edge proxy  →  this server
 *                                                        ↘ :3031 in-process gateway
 *
 * Guards:
 *  - nodejs runtime only (never the edge runtime)
 *  - never during `next build` (phase-production-build)
 *  - idempotent — see `bootEmbeddedGateway`
 */

export async function register(): Promise<void> {
  if (process.env.NEXT_RUNTIME !== "nodejs") return;
  if (process.env.NEXT_PHASE === "phase-production-build") return;
  if (process.env.VENUE_GATEWAY_EMBED === "0") return;

  try {
    const { bootEmbeddedGateway } = await import("@perp/gateway/embed");
    await bootEmbeddedGateway({ port: 3031, host: "::" });
  } catch (err) {
    // The terminal still works in sim mode — never block server startup.
    console.error("[instrumentation] venue gateway boot failed:", err);
  }
}
