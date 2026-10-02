/**
 * Composition root — one HTTP server, two planes:
 *   REST (signed) + socket.io venue stream, both wire-encoded.
 * Graceful shutdown drains sockets, stops the venue and persists keys.
 */

import { createServer, type Server as HttpServer } from "node:http";
import { loadConfig } from "./config";
import { CredentialStore } from "./credentials";
import { createRestRouter } from "./http";
import { setLogLevel, log } from "./log";
import { VenueHost } from "./venue";
import { createVenueSocket } from "./socket";

export interface Gateway {
  server: HttpServer;
  host: VenueHost;
  creds: CredentialStore;
  port: number;
  close(): Promise<void>;
}

export function createGateway(overrides?: Partial<Record<string, string>>): Gateway {
  for (const [k, v] of Object.entries(overrides ?? {})) process.env[k] = v;
  const config = loadConfig();
  setLogLevel(config.logLevel);

  const host = new VenueHost(config);
  const creds = new CredentialStore(config);

  const server = createServer(createRestRouter({ config, host, creds }));
  const io = createVenueSocket(server, { config, host, creds });

  server.on("clientError", (err, socket) => {
    log.debug("http.client_error", { error: String(err) });
    socket.end("HTTP/1.1 400 Bad Request\r\n\r\n");
  });

  const gateway: Gateway = {
    server,
    host,
    creds,
    port: config.port,
    close(): Promise<void> {
      return new Promise((resolve) => {
        // Force-close live sockets — tests and rolling deploys both need it.
        io.disconnectSockets(true);
        io.close(() => {
          server.close(() => {
            host.stop();
            log.info("gateway.stopped", { uptimeMs: Date.now() - host.stats.startedAt });
            resolve();
          });
        });
        // Hard stop for lingering connections after 5s.
        setTimeout(() => resolve(), 5_000).unref();
      });
    },
  };

  return gateway;
}

export function run(): void {
  const config = loadConfig();
  const gateway = createGateway();
  gateway.host.start();
  gateway.server.listen(config.port, config.host, () => {
    log.info("gateway.listening", {
      host: config.host,
      port: config.port,
      authRequired: config.authRequired,
      cors: config.corsOrigins,
      transport: "socket.io + REST",
    });
  });

  const shutdown = (signal: string) => {
    log.info("gateway.shutdown", { signal });
    void gateway.close().then(() => process.exit(0));
  };
  process.on("SIGTERM", () => shutdown("SIGTERM"));
  process.on("SIGINT", () => shutdown("SIGINT"));
}
