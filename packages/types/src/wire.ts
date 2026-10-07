/**
 * Wire codec — JSON frames that carry bigint money.
 *
 * Every venue payload (commands, events, snapshots, book updates) contains
 * bigint minor units. `JSON.stringify` throws on bigint; `postMessage`
 * carries it natively. The wire codec is the bridge for everything that is
 * NOT structured-clone: the socket.io gateway and the signed REST surface.
 *
 * Encoding: bigint → { "$bigint": "<decimal>" } markers, decoded back to
 * bigint on arrival. Everything else passes through untouched. Round-trip
 * stable, dependency-free, and human-inspectable in logs.
 */

const MARKER = "$bigint";

type BigintMarker = { [k: string]: string };

function isMarker(v: unknown): v is BigintMarker {
  return (
    typeof v === "object" &&
    v !== null &&
    !Array.isArray(v) &&
    Object.keys(v).length === 1 &&
    (v as BigintMarker)[MARKER] !== undefined &&
    typeof (v as BigintMarker)[MARKER] === "string"
  );
}

function encode(v: unknown): unknown {
  if (typeof v === "bigint") return { [MARKER]: v.toString(10) };
  if (Array.isArray(v)) return v.map(encode);
  if (v instanceof Map) {
    const out: Record<string, unknown> = {};
    for (const [k, val] of v) out[String(k)] = encode(val);
    return out;
  }
  if (v instanceof Set) return [...v].map(encode);
  if (v !== null && typeof v === "object") {
    const out: Record<string, unknown> = {};
    for (const [k, val] of Object.entries(v)) out[k] = encode(val);
    return out;
  }
  return v;
}

function decode(v: unknown): unknown {
  if (isMarker(v)) return BigInt(v[MARKER]);
  if (Array.isArray(v)) return v.map(decode);
  if (v !== null && typeof v === "object") {
    const out: Record<string, unknown> = {};
    for (const [k, val] of Object.entries(v)) out[k] = decode(val);
    return out;
  }
  return v;
}

/** Serialize a payload with bigint markers. */
export function stringifyWire(payload: unknown): string {
  return JSON.stringify(encode(payload));
}

/** Parse a wire frame back to bigint-bearing values. Returns `fallback` on malformed input. */
export function parseWire<T>(frame: string): T {
  return decode(JSON.parse(frame)) as T;
}

/** Safe parse — never throws; `fallback` on malformed input (malformed frames are dropped upstream). */
export function tryParseWire<T>(frame: unknown, fallback: T): T {
  if (typeof frame !== "string") return frame as T;
  try {
    return parseWire<T>(frame);
  } catch {
    return fallback;
  }
}

/**
 * G-25 canonical body hash — the `body_hash` slot of the signed request
 * canonical string. FNV-derived (deterministic, dependency-free, identical
 * in browser and server runtimes). Shared by BOTH sides of the wire so the
 * canonical form can never drift.
 */
export function bodyHashHex(body: string): string {
  let h1 = 0x811c9dc5;
  let h2 = 0x01000193;
  for (let i = 0; i < body.length; i++) {
    h1 = Math.imul(h1 ^ body.charCodeAt(i), 0x85ebca6b) >>> 0;
    h2 = Math.imul(h2 + body.charCodeAt(i), 0xc2b2ae35) >>> 0;
  }
  return (h1.toString(16).padStart(8, "0") + h2.toString(16).padStart(8, "0")).padEnd(64, "0");
}
