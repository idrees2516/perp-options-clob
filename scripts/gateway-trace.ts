import { createGateway } from "../apps/gateway/src/server.ts";
import { io } from "socket.io-client";
import { stringifyWire, parseWire } from "../packages/types/src/index.ts";
import type { Command } from "../packages/types/src/index.ts";

const gateway = createGateway({ GATEWAY_PORT: "3995", GATEWAY_HOST: "127.0.0.1", GATEWAY_VENUE_SEED: "4242", GATEWAY_VENUE_SPEED: "60" });
gateway.host.start();
await new Promise<void>((r) => gateway.server.listen(3995, "127.0.0.1", r));
console.log("gateway up");

const socket = io("http://127.0.0.1:3995", { transports: ["websocket"], reconnection: false, timeout: 5000 });
await new Promise<void>((r, rej) => { socket.on("connect", r); socket.on("connect_error", rej); });

const seen: string[] = [];
socket.on("venue-message", (frame: string) => {
  const msg = parseWire<{ channel: string; message?: string }>(frame);
  seen.push(msg.channel);
  if (msg.channel === "error") console.log("ERROR FRAME:", msg.message);
  if (msg.channel === "trade_fill") console.log("FILL RECEIVED");
});

socket.emit("venue-control", stringifyWire({ type: "connect" }));
await new Promise((r) => setTimeout(r, 1500));

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
socket.emit("venue-control", stringifyWire({ type: "command", command: cmd }));
await new Promise((r) => setTimeout(r, 2000));

console.log("channels seen:", JSON.stringify(seen.reduce((acc: Record<string, number>, c: string) => { acc[c] = (acc[c] ?? 0) + 1; return acc; }, {})));
socket.disconnect();
await gateway.close();
process.exit(0);
