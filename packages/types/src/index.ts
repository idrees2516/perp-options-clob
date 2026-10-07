/**
 * @perp/types — domain types mirroring the perp-options-clob Rust engine 1:1.
 *
 * Layered:
 *   primitives  — aliases, sides, TIF, order types
 *   money       — exact bigint arithmetic + formatting (f64 never touches a ledger)
 *   instrument  — PerpMarket / OptionMarket / marks
 *   order       — OrderRequest / Order / Rejection
 *   command     — all 28 engine Commands
 *   event       — all 57 engine Events + journal entries
 *   account     — positions, margin summaries, health
 *   market      — books, snapshots/deltas, oracle state
 *   economics   — fee ladder, revenue router, MM tiers, funding
 *   protocol    — withdrawals, governance, vaults, PoR, RFQ, MMP, settlement
 *   venue       — aggregated UI view-models
 *   transport   — UI ⇄ venue message contract (worker / gateway)
 */

export * from "./primitives";
export * from "./money";
export * from "./instrument";
export * from "./order";
export * from "./command";
export * from "./event";
export * from "./account";
export * from "./market";
export * from "./economics";
export * from "./protocol";
export * from "./venue";
export * from "./transport";
export * from "./wire";
