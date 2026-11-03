//! The shard engine: one underlying's complete 0DTE options market.
//!
//! A [`Shard`] owns every option series on one underlying, every account's
//! sub-ledger for that underlying, the risk engine, the expiry scheduler,
//! the circuit breaker and the command journal. A single `execute` call runs
//! the whole pipeline:
//!
//! ```text
//! rate-limit -> phase/band checks -> position/delta/margin gates
//!   -> FOK pre-scan -> match loop (STP-aware) -> per-trade settlement
//!   -> commitment bookkeeping -> remainder resting or cancel
//! ```
//!
//! Steady state performs no allocation: books, slabs, the event buffer and
//! the journal batch are all pre-reserved and recycled.
//!
//! Money is exact-integer double-entry: `sum(account.cash) + house ==
//! total_deposits` holds after **every** command.

use crate::accounts::{signed, Account, Ledger, Position, TokenBucket};
use crate::auction;
use crate::book::{make_order, Book, NIL};
use crate::expiry::{Action, Wheel};
use crate::fees::{FeeSchedule, Tier};
use crate::journal::{CmdLog, EvBuf, Event};
use crate::margin::{self, RiskParams};
use crate::symbol::Listing;
use crate::types::*;

/// Nanoseconds per minute.
pub const NANOS_PER_MIN: u64 = 60_000_000_000;

// ----------------------------------------------------------------------
// Configuration
// ----------------------------------------------------------------------

/// Venue configuration for one shard.
#[derive(Clone, Debug)]
pub struct ShardConfig {
    /// Options margin/risk parameters.
    pub risk: RiskParams,
    /// Fee schedule.
    pub fees: FeeSchedule,
    /// Option price band around its last mark, basis points (2000 = 20%).
    pub band_bps: u32,
    /// Tightened band in fast market, basis points (500 = 5%).
    pub fast_band_bps: u32,
    /// Minutes before expiry at which the MOC auction window opens.
    pub auction_window_min: u64,
    /// Minutes an auction stays open after a circuit-breaker reopen.
    pub reopen_window_min: u64,
    /// Halt duration in nanoseconds after a circuit break.
    pub halt_nanos: u64,
    /// Circuit-breaker trigger vs session reference, basis points (700 = 7%).
    pub circuit_bps: u32,
    /// Fast-market trigger, basis points (300 = 3%).
    pub fast_bps: u32,
    /// Per-book order capacity hint.
    pub order_cap: usize,
    /// Per-book level capacity hint.
    pub level_cap: usize,
    /// Per-account cap on simultaneously resting orders across the shard
    /// (book-flood / memory-exhaustion defense).
    pub max_open_orders: u32,
    /// Maximum quantity of a single order (fat-finger defense).
    pub max_order_qty: u32,
    /// Keep the WAL in memory (true, default) or discard flushed frames
    /// (false — pure engine-capacity mode for benchmarking).
    pub journal_in_memory: bool,
}

impl ShardConfig {
    /// Production-flavoured defaults.
    pub fn default_config() -> ShardConfig {
        ShardConfig {
            risk: RiskParams::default_params(),
            fees: FeeSchedule::default_schedule(),
            band_bps: 2000,
            fast_band_bps: 500,
            auction_window_min: 10,
            reopen_window_min: 1,
            halt_nanos: 5 * NANOS_PER_MIN,
            circuit_bps: 700,
            fast_bps: 300,
            order_cap: 4096,
            level_cap: 256,
            max_open_orders: 256,
            max_order_qty: 10_000,
            journal_in_memory: true,
        }
    }
}

/// Trading phase of a symbol.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    /// Normal continuous matching.
    Continuous,
    /// Collecting auction interest; no crossing.
    AuctionOpen,
    /// Expired and settled; dead.
    Expired,
}

/// Per-symbol live state.
pub struct SymbolState {
    /// The listing.
    pub listing: Listing,
    /// Order book (also holds auction interest while in auction phase).
    pub book: Book,
    /// Phase.
    pub phase: Phase,
    /// Last trade price (option mark).
    pub mark: Price,
    /// Settlement price once expired.
    pub settle: Price,
    /// Session open.
    pub open: Price,
    /// Session high.
    pub high: Price,
    /// Session low.
    pub low: Price,
    /// Last trade price.
    pub last: Price,
    /// Traded volume (contracts).
    pub vol: u64,
    /// VWAP numerator (price*qty sum).
    pub vwap_num: u128,
    /// Trades count.
    pub n_trades: u64,
    /// Cached BBO for delta emission.
    pub bbo: (Price, u64, Price, u64),
    /// Cached delta (bps) at the current underlying reference — updated on
    /// oracle ticks; kills per-order variable divisions on the hot path.
    pub delta_bps: i64,
}

impl SymbolState {
    fn new(listing: Listing, cfg: &ShardConfig) -> SymbolState {
        SymbolState {
            listing,
            book: Book::with_capacity(cfg.order_cap, cfg.level_cap),
            phase: Phase::Continuous,
            mark: 0,
            settle: 0,
            open: 0,
            high: 0,
            low: 0,
            last: 0,
            vol: 0,
            vwap_num: 0,
            n_trades: 0,
            bbo: (0, 0, 0, 0),
            delta_bps: listing.delta_bps(0),
        }
    }

    /// Volume-weighted average price, if any volume.
    pub fn vwap(&self) -> Option<Price> {
        if self.vol == 0 {
            None
        } else {
            Some((self.vwap_num / self.vol as u128) as Price)
        }
    }
}

// ----------------------------------------------------------------------
// Commands
// ----------------------------------------------------------------------

/// Journalable input command.
#[derive(Clone, Copy, Debug)]
pub enum Cmd {
    /// Submit an order (symbol is the shard-local symbol id).
    Place(NewOrder),
    /// Cancel an order by id and symbol. The cancelling account must own
    /// the order — ownership is verified and `Reject::NotOwner` emitted on
    /// mismatch (authorization enforced in the engine, not just the edge).
    Cancel {
        /// Order to cancel (packed id).
        order: OrderId,
        /// Its symbol.
        symbol: SymbolId,
        /// Account requesting the cancel (must own the order).
        account: AcctId,
    },
    /// Cancel-replace: atomically cancel and submit a new order.
    Replace {
        /// Order to replace.
        order: OrderId,
        /// Its symbol.
        symbol: SymbolId,
        /// New price.
        price: Price,
        /// New quantity.
        qty: Qty,
        /// Account requesting the replace (must own the order).
        account: AcctId,
    },
    /// Register an account.
    CreateAccount {
        /// Account id.
        account: AcctId,
        /// Fee tier.
        tier: Tier,
    },
    /// Deposit cash.
    Deposit {
        /// Account id.
        account: AcctId,
        /// Amount in ticks.
        amount: Cash,
    },
    /// Withdraw cash.
    Withdraw {
        /// Account id.
        account: AcctId,
        /// Amount in ticks.
        amount: Cash,
    },
    /// List a new option series.
    CreateSymbol {
        /// The listing.
        listing: Listing,
    },
    /// Underlying mark update (oracle).
    Oracle {
        /// Underlying price in ticks.
        price: Price,
    },
    /// Advance logical time.
    Advance {
        /// New logical time (nanos).
        to: Nanos,
    },
    /// Manual halt.
    Halt,
    /// Manual resume (reopening auctions).
    ResumeNow,
}

/// Decode one journal record (see `journal_cmd` for the schema:
/// `[tag u8][fields...]`). Returns `None` on malformed input.
pub fn decode_cmd(raw: &[u8]) -> Option<Cmd> {
    if raw.is_empty() {
        return None;
    }
    let g32 = |o: usize| -> Option<u32> {
        raw.get(o..o + 4)
            .and_then(|b| b.try_into().ok())
            .map(u32::from_le_bytes)
    };
    let g64 = |o: usize| -> Option<u64> {
        raw.get(o..o + 8)
            .and_then(|b| b.try_into().ok())
            .map(u64::from_le_bytes)
    };
    let gcash = |o: usize| -> Option<Cash> {
        let lo = g64(o)? as u128;
        let hi = g64(o + 8)? as u128;
        Some((lo | (hi << 64)) as Cash)
    };
    match raw[0] {
        1 => Some(Cmd::Place(NewOrder {
            symbol: g32(1)?,
            account: g32(5)?,
            side: if raw[9] == 0 { Side::Buy } else { Side::Sell },
            tif: match raw[10] {
                0 => Tif::Gtc,
                1 => Tif::Ioc,
                _ => Tif::Fok,
            },
            kind: if raw[11] == 0 { Kind::Limit } else { Kind::Market },
            flags: *raw.get(12)?,
            price: g64(13)?,
            qty: g32(21)?,
            oco: g64(25)?,
        })),
        2 => {
            let order = g64(1)?;
            let symbol = g32(9)?;
            let account = g32(13)?;
            Some(Cmd::Cancel {
                order,
                symbol,
                account,
            })
        }
        3 => {
            let order = g64(1)?;
            let symbol = g32(9)?;
            let price = g64(13)?;
            let qty = g32(21)?;
            let account = g32(25)?;
            Some(Cmd::Replace {
                order,
                symbol,
                price,
                qty,
                account,
            })
        }
        4 => Some(Cmd::CreateAccount {
            account: g32(1)?,
            tier: match raw.get(5)? {
                1 => Tier::Silver,
                2 => Tier::Gold,
                3 => Tier::Prime,
                _ => Tier::Base,
            },
        }),
        5 => Some(Cmd::Deposit {
            account: g32(1)?,
            amount: gcash(5)?,
        }),
        6 => Some(Cmd::Withdraw {
            account: g32(1)?,
            amount: gcash(5)?,
        }),
        7 => Some(Cmd::CreateSymbol {
            listing: crate::symbol::Listing::unpack(g64(1)?),
        }),
        8 => Some(Cmd::Oracle { price: g64(1)? }),
        9 => Some(Cmd::Advance { to: g64(1)? }),
        10 => Some(Cmd::Halt),
        11 => Some(Cmd::ResumeNow),
        _ => None,
    }
}


// ----------------------------------------------------------------------
// Small open-addressing map: listing key -> local symbol id
// ----------------------------------------------------------------------

struct KeyMap {
    keys: Vec<u64>,
    vals: Vec<u32>,
    mask: usize,
    used: usize,
}

impl KeyMap {
    fn new(cap_pow2: usize) -> KeyMap {
        KeyMap {
            keys: vec![u64::MAX; cap_pow2],
            vals: vec![NIL; cap_pow2],
            mask: cap_pow2 - 1,
            used: 0,
        }
    }

    fn get(&self, k: u64) -> u32 {
        let mut i = (k.wrapping_mul(0x9E37_79B9_7F4A_7C15) as usize) & self.mask;
        loop {
            if self.keys[i] == k {
                return self.vals[i];
            }
            if self.keys[i] == u64::MAX {
                return NIL;
            }
            i = (i + 1) & self.mask;
        }
    }

    fn insert(&mut self, k: u64, v: u32) {
        if self.used * 2 >= self.mask {
            self.grow();
        }
        let mut i = (k.wrapping_mul(0x9E37_79B9_7F4A_7C15) as usize) & self.mask;
        while self.keys[i] != u64::MAX && self.keys[i] != k {
            i = (i + 1) & self.mask;
        }
        if self.keys[i] == u64::MAX {
            self.used += 1;
        }
        self.keys[i] = k;
        self.vals[i] = v;
    }

    fn grow(&mut self) {
        let n = (self.mask + 1) * 2;
        let old_keys = std::mem::replace(&mut self.keys, vec![u64::MAX; n]);
        let old_vals = std::mem::replace(&mut self.vals, vec![NIL; n]);
        self.mask = n - 1;
        self.used = 0;
        for (k, v) in old_keys.into_iter().zip(old_vals) {
            if k != u64::MAX {
                self.insert(k, v);
            }
        }
    }
}

// ----------------------------------------------------------------------
// Shard
// ----------------------------------------------------------------------

/// Aggregate per-shard statistics.
#[derive(Clone, Copy, Debug, Default)]
pub struct ShardStats {
    /// Orders accepted.
    pub orders: u64,
    /// Orders rejected.
    pub rejects: u64,
    /// Trades executed.
    pub trades: u64,
    /// Contracts traded.
    pub volume: u64,
    /// Premium traded (ticks).
    pub premium: u128,
    /// Fees collected by the house (ticks).
    pub fees: u64,
    /// Cancels processed.
    pub cancels: u64,
    /// Commands executed.
    pub cmds: u64,
    /// Symbols settled at expiry.
    pub expiries: u64,
    /// Auctions cleared.
    pub auctions: u64,
}

/// Position ownership metadata, parallel to the position slab.
#[derive(Clone, Copy, Debug)]
pub struct PositionMeta {
    /// Owning account.
    pub account: AcctId,
    /// Symbol.
    pub symbol: SymbolId,
}

/// Audit snapshot of shard integrity.
#[derive(Clone, Copy, Debug)]
pub struct AuditReport {
    /// `sum(cash) + house == deposits`.
    pub conservation_ok: bool,
    /// Sum of account cash.
    pub sum_cash: Cash,
    /// House balance.
    pub house: Cash,
    /// Total deposits.
    pub deposits: Cash,
    /// Live accounts.
    pub live_accounts: usize,
    /// Non-flat positions.
    pub open_positions: usize,
    /// Listed symbols.
    pub symbols: usize,
    /// Resting orders.
    pub book_orders: u64,
    /// Sum of cash holds.
    pub hold_sum: Cash,
    /// Sum of margin requirements.
    pub margin_required_sum: u64,
    /// Sum of margin reserves.
    pub margin_reserved_sum: u64,
    /// Accounts with negative cash (gap risk beyond margin).
    pub negative_cash: usize,
    /// Every account's margin_required matches its positions.
    pub margin_consistent: bool,
}

/// One underlying's complete market.
pub struct Shard {
    /// Shard identity.
    pub shard_id: u32,
    /// The underlying this shard trades.
    pub underlying: UnderlyingId,
    /// Configuration.
    pub cfg: ShardConfig,
    /// Accounts indexed by global account id (id == NIL marks holes).
    pub accounts: Vec<Account>,
    /// Rate limiter buckets, aligned with `accounts`.
    pub buckets: Vec<TokenBucket>,
    /// Position slab (per account+symbol).
    pub positions: Vec<Position>,
    /// Ownership metadata, parallel to `positions`.
    pub pos_meta: Vec<PositionMeta>,
    /// Free position slots.
    pos_free: Vec<u32>,
    /// Cached per-account delta notional.
    acct_delta: Vec<i64>,
    /// Resting-order count per account (book-flood defense).
    open_orders: Vec<u32>,
    /// Slots of positions currently short (margin-relevant); maintained on
    /// every position transition so oracle refreshes stay O(active shorts).
    short_slots: Vec<u32>,
    /// Round-robin cursor for budgeted incremental margin refreshes.
    short_cursor: usize,
    /// Reusable journal serialization scratch (fields written contiguously;
    /// only the first `n` bytes are ever copied out).
    scratch: [u8; 96],
    /// Symbols indexed by local symbol id.
    pub symbols: Vec<SymbolState>,
    /// listing key -> local symbol id.
    sym_map: KeyMap,
    /// The exact-integer ledger.
    pub ledger: Ledger,
    /// Logical clock (nanos).
    pub clock: Nanos,
    /// Monotonic sequence.
    pub seq: Seq,
    /// Underlying session reference price (first oracle of the day).
    pub ref_price: Price,
    /// Underlying last mark.
    pub mark_price: Price,
    /// Fast market state.
    pub fast: bool,
    /// Circuit halt state.
    pub halted: bool,
    /// Logical time at which the halt lifts.
    pub halt_until: Nanos,
    /// Expiry/auction scheduler.
    pub wheel: Wheel,
    /// Command journal (WAL).
    pub journal: CmdLog,
    /// Journaling enabled (disabled during replay).
    pub journaling: bool,
    /// Admission counter (taker references in trade events).
    admit: u64,
    /// Most recently acked (resting) order id — O(1) access for clients.
    last_ack_id: OrderId,
    /// Last minute at which margins were refreshed.
    last_margin_min: u64,
    /// Statistics.
    pub stats: ShardStats,
}

impl Shard {
    /// A fresh shard.
    pub fn new(shard_id: u32, underlying: UnderlyingId, cfg: ShardConfig) -> Shard {
        let in_mem = cfg.journal_in_memory;
        Shard {
            shard_id,
            underlying,
            cfg,
            accounts: Vec::new(),
            buckets: Vec::new(),
            positions: Vec::with_capacity(1024),
            pos_meta: Vec::with_capacity(1024),
            pos_free: Vec::new(),
            acct_delta: Vec::new(),
            open_orders: Vec::new(),
            short_slots: Vec::new(),
            short_cursor: 0,
            scratch: [0; 96],
            symbols: Vec::new(),
            sym_map: KeyMap::new(1 << 12),
            ledger: Ledger::empty(),
            clock: 0,
            seq: 0,
            ref_price: 0,
            mark_price: 0,
            fast: false,
            halted: false,
            halt_until: 0,
            wheel: Wheel::new(1024),
            journal: if in_mem {
                CmdLog::new(1024 * 1024)
            } else {
                CmdLog::discarding()
            },
            journaling: true,
            admit: 0,
            last_ack_id: 0,
            last_margin_min: 0,
            stats: ShardStats::default(),
        }
    }

    /// Current logical time.
    pub fn now(&self) -> Nanos {
        self.clock
    }

    /// Most recently acked resting order id (0 = none) — lets callers track
    /// their orders without scanning event batches.
    #[inline(always)]
    pub fn last_ack(&self) -> OrderId {
        self.last_ack_id
    }

    // ------------------------------------------------------------------
    // Command dispatch
    // ------------------------------------------------------------------

    /// Execute a batch of commands, accumulating all events into `ev`
    /// (the realistic feed-batching path: one dispatch amortized over the
    /// whole batch). Equivalent to calling [`Self::execute`] per command
    /// with the buffer cleared between, but cheaper per command.
    pub fn execute_batch(&mut self, cmds: &[Cmd], ev: &mut EvBuf) {
        for c in cmds {
            self.execute(*c, ev);
        }
    }

    /// Execute one command, appending events to `ev`.
    pub fn execute(&mut self, cmd: Cmd, ev: &mut EvBuf) {
        self.stats.cmds += 1;
        if self.journaling {
            self.journal_cmd(&cmd);
        }
        match cmd {
            Cmd::Place(o) => self.place(o, ev),
            Cmd::Cancel {
                order,
                symbol,
                account,
            } => {
                let slot = id_slot(order);
                // Ownership is enforced inside the (single) slot lookup; a
                // vanished order must still answer UnknownOrder.
                let before = ev.events.len();
                self.cancel_slot(slot, order, symbol, 0, ev, true, account);
                if ev.events.len() == before {
                    ev.push(Event::Reject {
                        order,
                        account,
                        symbol,
                        code: Reject::UnknownOrder as u8,
                    });
                }
            }
            Cmd::Replace {
                order,
                symbol,
                price,
                qty,
                account,
            } => self.replace(order, symbol, price, qty, account, ev),
            Cmd::CreateAccount { account, tier } => self.create_account(account, tier),
            Cmd::Deposit { account, amount } => self.deposit(account, amount, ev),
            Cmd::Withdraw { account, amount } => self.withdraw(account, amount, ev),
            Cmd::CreateSymbol { listing } => self.create_symbol(listing, ev),
            Cmd::Oracle { price } => self.oracle(price, ev),
            Cmd::Advance { to } => self.advance(to, ev),
            Cmd::Halt => self.enter_halt(7, ev),
            Cmd::ResumeNow => self.resume(ev),
        }
    }

    /// Serialize a command into the journal batch (stack buffer, no alloc).
    /// Auto-flushes when the pending batch crosses 256 KiB so the WAL stays
    /// bounded and no reallocation spikes reach the hot path.
    fn journal_cmd(&mut self, cmd: &Cmd) {
        if self.journal.pending_len() >= 262_144 {
            self.journal.flush();
        }
        let Shard {
            scratch,
            journal,
            clock,
            seq,
            ..
        } = self;
        let _ = (clock, seq); // logical time & ordering come from the
                               // command stream itself (Advance/sequence);
                               // records stay minimal for hot-path cost.
        let mut n = 0usize;
        let tag = match cmd {
            Cmd::Place(_) => 1u8,
            Cmd::Cancel { .. } => 2,
            Cmd::Replace { .. } => 3,
            Cmd::CreateAccount { .. } => 4,
            Cmd::Deposit { .. } => 5,
            Cmd::Withdraw { .. } => 6,
            Cmd::CreateSymbol { .. } => 7,
            Cmd::Oracle { .. } => 8,
            Cmd::Advance { .. } => 9,
            Cmd::Halt => 10,
            Cmd::ResumeNow => 11,
        };
        scratch[n] = tag;
        n += 1;
        match *cmd {
            Cmd::Place(o) => {
                scratch[n..n + 4].copy_from_slice(&o.symbol.to_le_bytes());
                n += 4;
                scratch[n..n + 4].copy_from_slice(&o.account.to_le_bytes());
                n += 4;
                scratch[n] = o.side as u8;
                scratch[n + 1] = o.tif as u8;
                scratch[n + 2] = o.kind as u8;
                scratch[n + 3] = o.flags;
                n += 4;
                scratch[n..n + 8].copy_from_slice(&o.price.to_le_bytes());
                n += 8;
                scratch[n..n + 4].copy_from_slice(&o.qty.to_le_bytes());
                n += 4;
                scratch[n..n + 8].copy_from_slice(&o.oco.to_le_bytes());
                n += 8;
            }
            Cmd::Cancel { order, symbol, account } => {
                scratch[n..n + 8].copy_from_slice(&order.to_le_bytes());
                n += 8;
                scratch[n..n + 4].copy_from_slice(&symbol.to_le_bytes());
                n += 4;
                scratch[n..n + 4].copy_from_slice(&account.to_le_bytes());
                n += 4;
            }
            Cmd::Replace {
                order,
                symbol,
                price,
                qty,
                account,
            } => {
                scratch[n..n + 8].copy_from_slice(&order.to_le_bytes());
                n += 8;
                scratch[n..n + 4].copy_from_slice(&symbol.to_le_bytes());
                n += 4;
                scratch[n..n + 8].copy_from_slice(&price.to_le_bytes());
                n += 8;
                scratch[n..n + 4].copy_from_slice(&qty.to_le_bytes());
                n += 4;
                scratch[n..n + 4].copy_from_slice(&account.to_le_bytes());
                n += 4;
            }
            Cmd::CreateAccount { account, tier } => {
                scratch[n..n + 4].copy_from_slice(&account.to_le_bytes());
                n += 4;
                scratch[n] = tier as u8;
                n += 1;
            }
            Cmd::Deposit { account, amount } | Cmd::Withdraw { account, amount } => {
                scratch[n..n + 4].copy_from_slice(&account.to_le_bytes());
                n += 4;
                let m = amount as u128;
                scratch[n..n + 8].copy_from_slice(&(m as u64).to_le_bytes());
                n += 8;
                scratch[n..n + 8].copy_from_slice(&((m >> 64) as u64).to_le_bytes());
                n += 8;
            }
            Cmd::CreateSymbol { listing } => {
                scratch[n..n + 8].copy_from_slice(&listing.key().to_le_bytes());
                n += 8;
            }
            Cmd::Oracle { price } => {
                scratch[n..n + 8].copy_from_slice(&price.to_le_bytes());
                n += 8;
            }
            Cmd::Advance { to } => {
                scratch[n..n + 8].copy_from_slice(&to.to_le_bytes());
                n += 8;
            }
            Cmd::Halt | Cmd::ResumeNow => {}
        }
        journal.append_raw(&scratch[..n]);
    }

    /// Flush the journal into a CRC-framed WAL batch.
    pub fn flush_journal(&mut self) {
        self.journal.flush();
    }

    // ------------------------------------------------------------------
    // Admin operations
    // ------------------------------------------------------------------

    fn ensure_account_capacity(&mut self, account: AcctId) {
        if account as usize >= self.accounts.len() {
            let n = account as usize + 1;
            self.accounts.resize(n, Account::new(NIL, 0, Tier::Base));
            self.buckets.resize(
                n,
                TokenBucket::new(self.cfg.risk.rate_per_sec, self.clock),
            );
            self.acct_delta.resize(n, 0);
            self.open_orders.resize(n, 0);
        }
    }

    fn create_account(&mut self, account: AcctId, tier: Tier) {
        self.ensure_account_capacity(account);
        let a = &mut self.accounts[account as usize];
        a.id = account;
        a.tier = tier;
        a.cash = 0;
        a.hold = 0;
        a.margin_required = 0;
        a.margin_reserved = 0;
        a.fees_paid = 0;
        a.rebates_recv = 0;
        a.pos.clear();
        self.acct_delta[account as usize] = 0;
    }

    fn deposit(&mut self, account: AcctId, amount: Cash, ev: &mut EvBuf) {
        if account as usize >= self.accounts.len() || self.accounts[account as usize].id == NIL {
            return;
        }
        // Defensive validation: positive amounts only, checked arithmetic so
        // no overflow can ever break ledger conservation.
        if amount <= 0 {
            return;
        }
        let a = &mut self.accounts[account as usize];
        match a.cash.checked_add(amount) {
            Some(_) => a.cash += amount,
            None => return,
        }
        self.ledger.deposit(amount);
        ev.push(Event::Deposited { account, amount });
    }

    fn withdraw(&mut self, account: AcctId, amount: Cash, ev: &mut EvBuf) {
        if account as usize >= self.accounts.len() || self.accounts[account as usize].id == NIL {
            return;
        }
        if amount <= 0 {
            return;
        }
        if self.accounts[account as usize].available() >= amount {
            self.accounts[account as usize].cash -= amount;
            self.ledger.withdraw(amount);
            ev.push(Event::Withdrawn { account, amount });
        }
    }

    fn create_symbol(&mut self, listing: Listing, ev: &mut EvBuf) {
        let key = listing.key();
        if self.sym_map.get(key) != NIL {
            return;
        }
        let id = self.symbols.len() as u32;
        self.symbols.push(SymbolState::new(listing, &self.cfg));
        self.sym_map.insert(key, id);
        ev.push(Event::SymbolListed {
            symbol: id,
            listing_key: key,
        });
        // Schedule MOC auction open and settlement on the expiry wheel.
        let expiry_ns = listing.expiry_min as u64 * NANOS_PER_MIN;
        let now = self.clock;
        let window = self.cfg.auction_window_min * NANOS_PER_MIN;
        if expiry_ns > now {
            self.wheel
                .schedule(expiry_ns - window, Action::OpenAuction(id));
            self.wheel.schedule(expiry_ns, Action::Settle(id));
        }
    }

    /// Look up a local symbol id from a listing key.
    pub fn symbol_id(&self, listing: Listing) -> SymbolId {
        self.sym_map.get(listing.key())
    }

    // ------------------------------------------------------------------
    // Oracle & circuit breaker
    // ------------------------------------------------------------------

    fn oracle(&mut self, price: Price, ev: &mut EvBuf) {
        if price == 0 {
            return;
        }
        self.mark_price = price;
        ev.push(Event::UnderlyingMark {
            underlying: self.underlying,
            price,
        });
        if self.ref_price == 0 {
            self.ref_price = price;
        }
        // Fast market hysteresis.
        let half = self.cfg.fast_bps as u64 / 2;
        let off = self.ref_price.saturating_mul(10_000 - half) / 10_000;
        let on = self.ref_price.saturating_mul(10_000 + half) / 10_000;
        let was_fast = self.fast;
        self.fast = !(off..=on).contains(&price);
        if self.fast != was_fast {
            ev.push(Event::FastMarket {
                underlying: self.underlying,
                on: self.fast,
            });
        }
        // Circuit breaker.
        if !self.halted && self.ref_price > 0 {
            let lo = self.ref_price.saturating_mul(10_000 - self.cfg.circuit_bps as u64) / 10_000;
            let hi = self.ref_price.saturating_mul(10_000 + self.cfg.circuit_bps as u64) / 10_000;
            if price < lo || price > hi {
                let level = if price < lo { 1 } else { 2 };
                self.enter_halt(level, ev);
            }
        }
        // Incremental short-margin refresh against the new underlying mark
        // (bounded 256 positions per tick, round-robin).
        self.refresh_margins_budget(32);
        self.last_margin_min = self.clock / NANOS_PER_MIN;
    }

    fn enter_halt(&mut self, level: u8, ev: &mut EvBuf) {
        if self.halted {
            return;
        }
        self.halted = true;
        self.halt_until = self.clock + self.cfg.halt_nanos;
        ev.push(Event::Halted {
            underlying: self.underlying,
            level,
        });
    }

    fn resume(&mut self, ev: &mut EvBuf) {
        if !self.halted {
            return;
        }
        self.halted = false;
        ev.push(Event::Resumed {
            underlying: self.underlying,
        });
        // Reopening auction per active symbol (book carries over).
        let reopen = self.cfg.reopen_window_min * NANOS_PER_MIN;
        let at = self.clock + reopen;
        for id in 0..self.symbols.len() as u32 {
            if self.symbols[id as usize].phase == Phase::Continuous {
                self.symbols[id as usize].phase = Phase::AuctionOpen;
                ev.push(Event::AuctionOpened { symbol: id });
                self.wheel.schedule(at, Action::CloseAuction(id));
            }
        }
    }

    /// Recompute margin for active short positions — O(active shorts).
    /// Called on TTL-bucket boundaries (full sweep) and oracle ticks
    /// (budgeted round-robin sweep, see [`Self::refresh_margins_budget`]).
    pub fn refresh_margins(&mut self) {
        self.short_cursor = 0;
        self.refresh_margins_budget(usize::MAX);
    }

    /// Incremental margin refresh: recompute at most `budget` shorts,
    /// continuing from the previous sweep's cursor (round-robin). This is
    /// the production risk-engine pattern — bounded worst-case cost per
    /// oracle tick, full coverage across ticks.
    pub fn refresh_margins_budget(&mut self, budget: usize) {
        let ref_p = if self.mark_price > 0 {
            self.mark_price
        } else {
            self.ref_price
        };
        let now_min = self.clock / NANOS_PER_MIN;
        let risk = self.cfg.risk;
        let mut deltas: Vec<(AcctId, i64)> = Vec::new();
        let n = self.short_slots.len();
        if n == 0 {
            self.short_cursor = 0;
            return;
        }
        let mut done = 0usize;
        let mut idx = self.short_cursor % n;
        while done < budget.max(1) {
            let slot = self.short_slots[idx];
            let p = self.positions[slot as usize];
            if p.qty < 0 {
                let meta = self.pos_meta[slot as usize];
                let sym = &self.symbols[meta.symbol as usize];
                let ttl = (sym.listing.expiry_min as u64).saturating_sub(now_min);
                let new_margin = margin::short_margin_per_contract(
                    &risk,
                    sym.listing,
                    sym.mark,
                    ref_p,
                    ttl,
                ) * p.qty.unsigned_abs() as u64;
                let d = new_margin as i64 - p.margin as i64;
                if d != 0 {
                    self.positions[slot as usize].margin = new_margin;
                    deltas.push((meta.account, d));
                }
            }
            idx = (idx + 1) % n;
            done += 1;
            if done >= n {
                break; // one full sweep
            }
        }
        self.short_cursor = idx;
        for (acct, d) in deltas {
            let a = &mut self.accounts[acct as usize];
            a.margin_required = (a.margin_required as i64 + d).max(0) as u64;
        }
        // Refresh per-symbol cached deltas (one per listing; cheap).
        if ref_p > 0 {
            for sym in &mut self.symbols {
                sym.delta_bps = sym.listing.delta_bps(ref_p);
            }
        }
    }

    // ------------------------------------------------------------------
    // Order entry
    // ------------------------------------------------------------------

    fn reject(&mut self, o: &NewOrder, code: Reject, ev: &mut EvBuf) {
        self.stats.rejects += 1;
        self.admit += 1;
        ev.push(Event::Reject {
            order: self.admit,
            account: o.account,
            symbol: o.symbol,
            code: code as u8,
        });
    }

    fn place(&mut self, o: NewOrder, ev: &mut EvBuf) {
        self.admit += 1;
        if o.qty == 0 || o.price == 0 {
            self.reject(&o, Reject::Malformed, ev);
            return;
        }
        if o.qty > self.cfg.max_order_qty {
            self.reject(&o, Reject::QtyTooLarge, ev);
            return;
        }
        let sym = o.symbol as usize;
        if sym >= self.symbols.len() {
            self.reject(&o, Reject::UnknownSymbol, ev);
            return;
        }
        if self.halted {
            self.reject(&o, Reject::Halted, ev);
            return;
        }
        let phase = self.symbols[sym].phase;
        if phase == Phase::Expired {
            self.reject(&o, Reject::NotContinuous, ev);
            return;
        }
        let account = o.account as usize;
        if account >= self.accounts.len() || self.accounts[account].id == NIL {
            self.reject(&o, Reject::UnknownAccount, ev);
            return;
        }
        if !self.buckets[account].take(self.clock) {
            self.reject(&o, Reject::RateLimit, ev);
            return;
        }
        // Book-flood defense: cap simultaneously resting orders per account.
        if self.open_orders[account] >= self.cfg.max_open_orders
            && o.kind == Kind::Limit
            && o.tif == Tif::Gtc
        {
            self.reject(&o, Reject::TooManyOrders, ev);
            return;
        }
        let listing = self.symbols[sym].listing;
        let mark = self.symbols[sym].mark;
        let now_min = self.clock / NANOS_PER_MIN;
        let ttl = (listing.expiry_min as u64).saturating_sub(now_min);
        let ref_p = if self.mark_price > 0 {
            self.mark_price
        } else {
            self.ref_price
        };

        // Price band (continuous phase only; auctions discover price).
        if phase == Phase::Continuous && mark > 0 {
            let band_bps = if self.fast {
                self.cfg.fast_band_bps as u64
            } else {
                self.cfg.band_bps as u64
            };
            let lo = mark.saturating_mul(10_000 - band_bps) / 10_000;
            let hi = mark.saturating_mul(10_000 + band_bps) / 10_000;
            if o.price < lo || o.price > hi {
                self.reject(&o, Reject::PriceBand, ev);
                return;
            }
        }

        // Post-only: reject if it would cross the current best.
        if o.is_post_only() && phase == Phase::Continuous {
            let opp_best = self.symbols[sym].book.best_price(o.side.opp());
            let crosses = match o.side {
                Side::Buy => opp_best > 0 && o.price >= opp_best,
                Side::Sell => opp_best > 0 && o.price <= opp_best,
            };
            if crosses {
                self.reject(&o, Reject::PostOnlyCross, ev);
                return;
            }
        }

        // FOK pre-scan: all-or-nothing against available depth.
        if o.tif == Tif::Fok && phase == Phase::Continuous {
            let cum = self.symbols[sym].book.cum_within(o.side, o.price);
            if cum < o.qty as u64 {
                self.reject(&o, Reject::FokDepth, ev);
                return;
            }
        }

        // Resolve the position slot.
        let slot = self.resolve_slot(o.account, o.symbol);

        // Reduce-only gate.
        if o.is_reduce_only() {
            let cur = self.positions[slot as usize].qty;
            let delta = signed(o.side, o.qty);
            let ok = cur != 0 && cur.signum() == -delta.signum() && delta.abs() <= cur.abs();
            if !ok {
                self.reject(&o, Reject::ReduceOnlyWouldIncrease, ev);
                return;
            }
        }

        // Position limit.
        let cur_qty = self.positions[slot as usize].qty;
        let post_qty = cur_qty + signed(o.side, o.qty);
        if post_qty.unsigned_abs() > self.cfg.risk.position_limit {
            self.reject(&o, Reject::PositionLimit, ev);
            return;
        }

        // Delta notional limit (cached per-symbol delta; no divisions).
        let d_bps = self.symbols[sym].delta_bps;
        let dn_order = d_bps * signed(o.side, o.qty) as i64 * ref_p as i64 / 10_000;
        let dn_cur = d_bps * cur_qty as i64 * ref_p as i64 / 10_000;
        let post_dn = self.acct_delta[account] + dn_order - dn_cur;
        if post_dn.unsigned_abs() > self.cfg.risk.delta_notional_limit {
            self.reject(&o, Reject::DeltaLimit, ev);
            return;
        }

        // Funds / margin gates with per-contract commitments.
        let tier = self.accounts[account].tier;
        let taker_rate = self.cfg.fees.taker_rate(tier);
        let commit_pc = match o.side {
            Side::Buy => o.price + taker_rate,
            Side::Sell => margin::short_margin_per_contract(
                &self.cfg.risk,
                listing,
                o.price,
                ref_p,
                ttl,
            ),
        };
        {
            let a = &self.accounts[account];
            let need_cash = if o.side == Side::Buy {
                commit_pc * o.qty as u64
            } else {
                0
            };
            let need_margin = if o.side == Side::Sell {
                commit_pc * o.qty as u64
            } else {
                0
            };
            let available = a.available();
            if available < need_cash as Cash + need_margin as Cash {
                self.reject(&o, Reject::InsufficientFunds, ev);
                return;
            }
        }

        self.seq += 1;

        // Commit buying power for the FULL order up front (released exactly
        // per filled / cancelled quantity). This prevents over-committing
        // the same cash to multiple resting orders.
        let (cc, mc) = commit_amounts(o.side, commit_pc, o.qty);
        self.accounts[account].commit(cc, mc);

        // Auction phase: straight to book, no crossing.
        if phase == Phase::AuctionOpen {
            let order = make_order(
                o.account,
                o.symbol,
                o.side,
                o.price,
                o.qty,
                o.tif,
                o.kind,
                o.flags,
                slot,
                id_slot(o.oco),
                id_uid(o.oco),
                commit_pc,
            );
            let oid = self.symbols[sym].book.insert(order);
            self.wire_oco(o.symbol, oid);
            self.open_orders[account] += 1;
            self.stats.orders += 1;
            self.last_ack_id = oid;
            ev.push(Event::Ack {
                order: oid,
                account: o.account,
                symbol: o.symbol,
                leaves: o.qty,
                buy: o.side == Side::Buy,
                price: o.price,
                qty: o.qty,
                tif: o.tif as u8,
                kind: o.kind as u8,
                flags: o.flags,
            });
            self.emit_bbo(o.symbol, ev);
            return;
        }

        // Continuous: cross what we can.
        let taker_ref = self.admit;
        let mut rem = o.qty;
        loop {
            match self.match_step(&o, rem, taker_ref, commit_pc, ev) {
                Step::Stop => break,
                Step::Filled(q) | Step::Stp(q) => {
                    rem -= q;
                    if rem == 0 {
                        break;
                    }
                }
            }
        }

        // Remainder handling.
        let mut ack_order = 0u64;
        if rem > 0 {
            let rests = o.kind == Kind::Limit && o.tif == Tif::Gtc;
            if rests {
                let order = make_order(
                    o.account,
                    o.symbol,
                    o.side,
                    o.price,
                    rem,
                    o.tif,
                    o.kind,
                    o.flags,
                    slot,
                    id_slot(o.oco),
                    id_uid(o.oco),
                    commit_pc,
                );
                ack_order = self.symbols[sym].book.insert(order);
                self.wire_oco(o.symbol, ack_order);
                self.open_orders[account] += 1;
                self.last_ack_id = ack_order;
            } else {
                // IOC/FOK/market remainder: release its commitment.
                let (rc, rm) = commit_amounts(o.side, commit_pc, rem);
                self.accounts[account].release(rc, rm);
            }
        }
        self.stats.orders += 1;
        ev.push(Event::Ack {
            order: ack_order,
            account: o.account,
            symbol: o.symbol,
            leaves: rem,
            buy: o.side == Side::Buy,
            price: o.price,
            qty: o.qty,
            tif: o.tif as u8,
            kind: o.kind as u8,
            flags: o.flags,
        });
        self.emit_bbo(o.symbol, ev);
    }

    /// Wire the OCO back-link: when order `id` names a live partner, point
    /// the partner back at `id` so cancelling either side cascades.
    fn wire_oco(&mut self, symbol: SymbolId, id: OrderId) {
        let slot = id_slot(id);
        if slot == NIL {
            return;
        }
        let (pslot, puid) = {
            let book = &self.symbols[symbol as usize].book;
            let o = book.orders[slot as usize];
            (o.oco, o.oco_uid)
        };
        if pslot == NIL || pslot == slot {
            return;
        }
        let book = &mut self.symbols[symbol as usize].book;
        if (pslot as usize) < book.orders.len() {
            let p = book.orders[pslot as usize];
            if p.uid == puid && p.level != NIL {
                book.orders[pslot as usize].oco = slot;
                book.orders[pslot as usize].oco_uid = id_uid(id);
            }
        }
    }

    /// One iteration of the match loop against the best opposite level.
    fn match_step(
        &mut self,
        taker: &NewOrder,
        rem: Qty,
        taker_ref: u64,
        taker_commit_pc: u64,
        ev: &mut EvBuf,
    ) -> Step {
        let sym = taker.symbol as usize;
        let opp = taker.side.opp();
        // Snapshot the best opposite level before mutating.
        let (lvl_price, head) = {
            let book = &self.symbols[sym].book;
            let lvl = book.best_level(opp);
            if lvl == NIL {
                return Step::Stop;
            }
            let l = &book.levels[lvl as usize];
            (l.price, l.head)
        };
        let crosses = match taker.side {
            Side::Buy => taker.price >= lvl_price,
            Side::Sell => taker.price <= lvl_price,
        };
        if !crosses {
            return Step::Stop;
        }
        let maker = self.symbols[sym].book.orders[head as usize];

        // Self-trade prevention: DecrementAndCancel.
        if maker.account == taker.account {
            let dec = rem.min(maker.leaves);
            self.cancel_slot(head, pack_id(head, maker.uid), taker.symbol, 1, ev, false, maker.account);
            // Release the taker's commitment for the decremented qty.
            let (cc, mc) = commit_amounts(taker.side, taker_commit_pc, dec);
            self.accounts[taker.account as usize].release(cc, mc);
            return Step::Stp(dec);
        }

        // Fill against the head maker.
        let q = rem.min(maker.leaves);
        let (maker, price, mslot) = self.symbols[sym].book.reduce_head(opp, q);
        self.settle_fill(
            taker.symbol,
            taker.side,
            taker.account,
            taker_ref,
            taker_commit_pc,
            &maker,
            mslot,
            price,
            q,
            ev,
        );
        Step::Filled(q)
    }

    fn resolve_slot(&mut self, account: AcctId, symbol: SymbolId) -> u32 {
        if let Some(s) = self.accounts[account as usize].pos_slot(symbol) {
            return s;
        }
        let slot = if let Some(s) = self.pos_free.pop() {
            self.positions[s as usize] = Position::FLAT;
            self.pos_meta[s as usize] = PositionMeta { account, symbol };
            s
        } else {
            let s = self.positions.len() as u32;
            self.positions.push(Position::FLAT);
            self.pos_meta.push(PositionMeta { account, symbol });
            s
        };
        self.accounts[account as usize].open_slot(symbol, slot);
        slot
    }

    /// Full per-trade settlement: premium, fees, commitments, positions.
    #[allow(clippy::too_many_arguments)]
    fn settle_fill(
        &mut self,
        symbol: SymbolId,
        taker_side: Side,
        taker_acct: AcctId,
        taker_ref: u64,
        taker_commit_pc: u64,
        maker: &crate::book::Order,
        maker_slot: u32,
        price: Price,
        qty: Qty,
        ev: &mut EvBuf,
    ) {
        let premium = price * qty as u64;
        let maker_acct = maker.account;
        // A maker consumed to zero leaves the book (reduce_head removed it).
        if maker.leaves == qty {
            self.open_orders[maker_acct as usize] =
                self.open_orders[maker_acct as usize].saturating_sub(1);
        }
        let taker_tier = self.accounts[taker_acct as usize].tier;
        let maker_tier = self.accounts[maker_acct as usize].tier;
        let fees = self.cfg.fees;
        let taker_fee = fees.taker_rate(taker_tier) * qty as u64;
        let maker_rate = fees.maker_rate(maker_tier) * qty as u64;

        // Premium exchange + fees (exact integer flows).
        if taker_side == Side::Buy {
            self.accounts[taker_acct as usize].cash -= premium as Cash + taker_fee as Cash;
            self.accounts[maker_acct as usize].cash += premium as Cash;
        } else {
            self.accounts[taker_acct as usize].cash += premium as Cash - taker_fee as Cash;
            self.accounts[maker_acct as usize].cash -= premium as Cash;
        }
        self.accounts[taker_acct as usize].fees_paid += taker_fee;
        self.ledger.fee_in(taker_fee);
        // Maker fee: rebate (house pays) or charge (house collects).
        let maker_fee_ev = maker_rate;
        let maker_rebate = fees.maker_is_rebate;
        if fees.maker_is_rebate {
            self.accounts[maker_acct as usize].cash += maker_rate as Cash;
            self.accounts[maker_acct as usize].rebates_recv += maker_rate;
            self.ledger.rebate_out(maker_rate);
        } else {
            self.accounts[maker_acct as usize].cash -= maker_rate as Cash;
            self.accounts[maker_acct as usize].fees_paid += maker_rate;
            self.ledger.fee_in(maker_rate);
        }

        // Release commitments for the filled quantity (exact reverse of the
        // admission-time commit).
        let (tcc, tmc) = commit_amounts(taker_side, taker_commit_pc, qty);
        self.accounts[taker_acct as usize].release(tcc, tmc);
        let (mcc, mmc) = commit_amounts(maker.side, maker.commit_pc, qty);
        self.accounts[maker_acct as usize].release(mcc, mmc);

        // Position updates with margin/delta caches.
        let (buyer, seller) = if taker_side == Side::Buy {
            (taker_acct, maker_acct)
        } else {
            (maker_acct, taker_acct)
        };
        let buy_slot = self.accounts[buyer as usize].pos_slot(symbol).unwrap_or(NIL);
        let sell_slot = self.accounts[seller as usize].pos_slot(symbol).unwrap_or(NIL);
        debug_assert!(buy_slot != NIL && sell_slot != NIL);
        self.apply_position_leg(buyer, buy_slot, symbol, qty as i32, price);
        self.apply_position_leg(seller, sell_slot, symbol, -(qty as i32), price);

        // Mark & session stats.
        {
            let s = &mut self.symbols[symbol as usize];
            if s.mark != price {
                s.mark = price;
                ev.push(Event::Mark { symbol, price });
            }
            if s.open == 0 {
                s.open = price;
            }
            if price > s.high {
                s.high = price;
            }
            if s.low == 0 || price < s.low {
                s.low = price;
            }
            s.last = price;
            s.vol += qty as u64;
            s.vwap_num += premium as u128;
            s.n_trades += 1;
        }
        self.stats.trades += 1;
        self.stats.volume += qty as u64;
        self.stats.premium += premium as u128;

        ev.push(Event::Trade {
            symbol,
            price,
            qty,
            taker: taker_ref,
            maker: pack_id(maker_slot, maker.uid),
            taker_acct,
            maker_acct,
            aggressor: taker_side as u8,
            taker_fee,
            maker_fee: maker_fee_ev,
            maker_rebate,
        });

        // Soft margin check on the short side.
        let short_acct = if taker_side == Side::Buy {
            maker_acct
        } else {
            taker_acct
        };
        let a = &self.accounts[short_acct as usize];
        if a.cash < a.margin_required as Cash {
            ev.push(Event::MarginDeficit {
                account: short_acct,
                required: a.margin_required,
                cash: a.cash,
            });
        }
    }

    fn apply_position_leg(
        &mut self,
        acct: AcctId,
        slot: u32,
        symbol: SymbolId,
        delta: i32,
        price: Price,
    ) {
        let meta = self.pos_meta[slot as usize];
        let listing = self.symbols[meta.symbol as usize].listing;
        let ref_p = if self.mark_price > 0 {
            self.mark_price
        } else {
            self.ref_price
        };
        let now_min = self.clock / NANOS_PER_MIN;
        let old = self.positions[slot as usize];
        let new_qty = old.qty + delta;
        let was_short = old.qty < 0;
        let is_short = new_qty < 0;
        if is_short != was_short {
            if is_short {
                self.short_slots.push(slot);
            } else if let Some(i) = self.short_slots.iter().position(|&s| s == slot) {
                self.short_slots.swap_remove(i);
            }
        }
        let ttl = (listing.expiry_min as u64).saturating_sub(now_min);
        let new_margin = if new_qty < 0 {
            margin::short_margin_per_contract(&self.cfg.risk, listing, price, ref_p, ttl)
                * new_qty.unsigned_abs() as u64
        } else {
            0
        };
        let new_dn = margin::delta_notional(listing, ref_p, new_qty);
        self.positions[slot as usize].qty = new_qty;
        self.positions[slot as usize].margin = new_margin;
        self.positions[slot as usize].delta_notional = new_dn;
        let a = &mut self.accounts[acct as usize];
        a.margin_required = (a.margin_required as i128 + new_margin as i128
            - old.margin as i128)
            .max(0) as u64;
        self.acct_delta[acct as usize] += new_dn - old.delta_notional;
        let _ = symbol;
    }

    // ------------------------------------------------------------------
    // Cancel / replace
    // ------------------------------------------------------------------

    /// Resolve the owning account of a live order, if any. Used for the
    /// engine-enforced authorization on cancel/replace.
    fn order_owner(&self, symbol: SymbolId, slot: u32, id: OrderId) -> Option<AcctId> {
        let sym = symbol as usize;
        if sym >= self.symbols.len() || slot == NIL || slot as usize >= self.symbols[sym].book.orders.len() {
            return None;
        }
        let o = self.symbols[sym].book.orders[slot as usize];
        if o.uid != id_uid(id) || o.level == NIL {
            return None;
        }
        Some(o.account)
    }

    /// Cancel by slot with generation guard; emits events, releases
    /// commitments, cascades OCO.
    #[allow(clippy::too_many_arguments)]
    fn cancel_slot(
        &mut self,
        slot: u32,
        id: OrderId,
        symbol: SymbolId,
        reason: u8,
        ev: &mut EvBuf,
        cascade_oco: bool,
        requester: AcctId,
    ) {
        self.cancel_slot_opt(slot, id, symbol, reason, ev, cascade_oco, true, requester)
    }

    /// Cancel with optional BBO emission (replaces suppress the transient
    /// between-cancel-and-replace quote).
    #[allow(clippy::too_many_arguments)]
    fn cancel_slot_opt(
        &mut self,
        slot: u32,
        id: OrderId,
        symbol: SymbolId,
        reason: u8,
        ev: &mut EvBuf,
        cascade_oco: bool,
        emit: bool,
        requester: AcctId,
    ) {
        let sym = symbol as usize;
        if sym >= self.symbols.len() || slot == NIL || slot as usize >= self.symbols[sym].book.orders.len() {
            return;
        }
        let book = &self.symbols[sym].book;
        let o = book.orders[slot as usize];
        if o.uid != id_uid(id) || o.level == NIL {
            return; // stale or recycled
        }
        // Engine-enforced authorization: internal cascades pass the order's
        // own owner; user cancels pass the requesting account.
        if o.account != requester {
            ev.push(Event::Reject {
                order: id,
                account: requester,
                symbol,
                code: Reject::NotOwner as u8,
            });
            return;
        }
        let o = self.symbols[sym].book.cancel(slot);
        self.stats.cancels += 1;
        self.open_orders[o.account as usize] = self.open_orders[o.account as usize].saturating_sub(1);
        let (cc, mc) = commit_amounts(o.side, o.commit_pc, o.leaves);
        self.accounts[o.account as usize].release(cc, mc);
        ev.push(Event::Canceled {
            order: pack_id(slot, o.uid),
            account: o.account,
            symbol,
            leaves: o.leaves,
            reason,
        });
        if cascade_oco && o.oco != NIL && o.oco != slot {
            let (pslot, puid) = (o.oco, o.oco_uid);
            self.cancel_slot_opt(pslot, pack_id(pslot, puid), symbol, 4, ev, false, true, requester);
        }
        if emit {
            self.emit_bbo(symbol, ev);
        }
    }

    fn replace(
        &mut self,
        order: OrderId,
        symbol: SymbolId,
        price: Price,
        qty: Qty,
        account: AcctId,
        ev: &mut EvBuf,
    ) {
        // Authorization: only the owner may replace.
        let slot = id_slot(order);
        match self.order_owner(symbol, slot, order) {
            Some(owner) if owner != account => {
                ev.push(Event::Reject {
                    order,
                    account,
                    symbol,
                    code: Reject::NotOwner as u8,
                });
                return;
            }
            None => {
                ev.push(Event::Reject {
                    order,
                    account,
                    symbol,
                    code: Reject::UnknownOrder as u8,
                });
                return;
            }
            Some(_) => {}
        }
        if qty == 0 || price == 0 || qty > self.cfg.max_order_qty {
            ev.push(Event::Reject {
                order,
                account,
                symbol,
                code: Reject::Malformed as u8,
            });
            return;
        }
        let sym = symbol as usize;
        if sym >= self.symbols.len() {
            ev.push(Event::Reject {
                order,
                account,
                symbol,
                code: Reject::UnknownSymbol as u8,
            });
            return;
        }
        let slot = id_slot(order);
        let book = &self.symbols[sym].book;
        if slot == NIL || slot as usize >= book.orders.len() {
            ev.push(Event::Reject {
                order,
                account,
                symbol,
                code: Reject::UnknownOrder as u8,
            });
            return;
        }
        let old = book.orders[slot as usize];
        if old.uid != id_uid(order) || old.level == NIL {
            ev.push(Event::Reject {
                order,
                account,
                symbol,
                code: Reject::UnknownOrder as u8,
            });
            return;
        }
        let (side, flags, tif, kind, account) = (old.side, old.flags, old.tif, old.kind, old.account);
        self.cancel_slot_opt(slot, order, symbol, 5, ev, true, false, account);
        let req = NewOrder {
            symbol,
            account,
            side,
            price,
            qty,
            tif,
            kind,
            flags,
            oco: NO_ID,
        };
        self.place(req, ev);
    }

    // ------------------------------------------------------------------
    // Auctions & expiry
    // ------------------------------------------------------------------

    /// Open the MOC auction window (resting continuous orders are pulled).
    fn open_auction(&mut self, symbol: SymbolId, ev: &mut EvBuf) {
        let sym = symbol as usize;
        if sym >= self.symbols.len() || self.symbols[sym].phase != Phase::Continuous {
            return;
        }
        self.symbols[sym].phase = Phase::AuctionOpen;
        ev.push(Event::AuctionOpened { symbol });
        // Pull resting continuous orders (reason 2 = expiry sweep),
        // releasing their commitments exactly.
        let mut pulled = Vec::new();
        self.symbols[sym].book.drain_all(&mut pulled);
        for (o, slot) in pulled {
            let (cc, mc) = commit_amounts(o.side, o.commit_pc, o.leaves);
            self.accounts[o.account as usize].release(cc, mc);
            self.open_orders[o.account as usize] =
                self.open_orders[o.account as usize].saturating_sub(1);
            ev.push(Event::Canceled {
                order: pack_id(slot, o.uid),
                account: o.account,
                symbol,
                leaves: o.leaves,
                reason: 2,
            });
        }
        self.emit_bbo(symbol, ev);
    }

    /// Reference price for auction tie-breaking: last trade mark, else the
    /// midpoint of the crossed book, else 1.
    fn auction_reference(s: &SymbolState) -> Price {
        if s.mark > 0 {
            return s.mark;
        }
        match (s.book.best_bid(), s.book.best_ask()) {
            (Some(b), Some(a)) => (b.0 + a.0) / 2,
            _ => 1,
        }
    }

    /// Close a reopening auction: clear at single price; leftovers rest.
    fn close_auction(&mut self, symbol: SymbolId, ev: &mut EvBuf) {
        let sym = symbol as usize;
        if sym >= self.symbols.len() || self.symbols[sym].phase != Phase::AuctionOpen {
            return;
        }
        let reference = Self::auction_reference(&self.symbols[sym]);
        let clearing = auction::clearing_price(&self.symbols[sym].book, reference);
        if clearing.qty > 0 {
            self.execute_auction_clear(symbol, clearing.price, clearing.qty, ev);
            ev.push(Event::AuctionCleared {
                symbol,
                price: clearing.price,
                qty: clearing.qty as Qty,
            });
            self.stats.auctions += 1;
        }
        self.symbols[sym].phase = Phase::Continuous;
        self.emit_bbo(symbol, ev);
    }

    /// Settle and expire a symbol: MOC auction, exercise, cleanup.
    fn settle_symbol(&mut self, symbol: SymbolId, ev: &mut EvBuf) {
        let sym = symbol as usize;
        if sym >= self.symbols.len() || self.symbols[sym].phase == Phase::Expired {
            return;
        }
        let listing = self.symbols[sym].listing;
        let reference = Self::auction_reference(&self.symbols[sym]);
        let clearing = auction::clearing_price(&self.symbols[sym].book, reference);
        if clearing.qty > 0 {
            self.execute_auction_clear(symbol, clearing.price, clearing.qty, ev);
            ev.push(Event::AuctionCleared {
                symbol,
                price: clearing.price,
                qty: clearing.qty as Qty,
            });
            self.stats.auctions += 1;
        }
        // Settlement price for exercise: the UNDERLYING settlement mark
        // (last oracle), never the option's own auction price. Options with
        // no underlying mark settle at strike (expire worthless).
        let settle = if self.mark_price > 0 {
            self.mark_price
        } else if self.ref_price > 0 {
            self.ref_price
        } else {
            listing.strike
        };
        // Cancel remaining auction interest, releasing commitments exactly.
        let mut pulled = Vec::new();
        self.symbols[sym].book.drain_all(&mut pulled);
        for (o, slot) in pulled {
            let (cc, mc) = commit_amounts(o.side, o.commit_pc, o.leaves);
            self.accounts[o.account as usize].release(cc, mc);
            self.open_orders[o.account as usize] =
                self.open_orders[o.account as usize].saturating_sub(1);
            ev.push(Event::Canceled {
                order: pack_id(slot, o.uid),
                account: o.account,
                symbol,
                leaves: o.leaves,
                reason: 2,
            });
        }
        // Cash-settle every position.
        self.exercise_positions(symbol, settle, ev);
        self.symbols[sym].phase = Phase::Expired;
        self.symbols[sym].settle = settle;
        self.stats.expiries += 1;
        ev.push(Event::Expired { symbol, settle });
        self.emit_bbo(symbol, ev);
    }

    /// Execute the auction allocation at `price` for `qty_total` contracts
    /// (price-time priority on both sides, maker fees both sides).
    fn execute_auction_clear(
        &mut self,
        symbol: SymbolId,
        price: Price,
        qty_total: u64,
        ev: &mut EvBuf,
    ) {
        let sym = symbol as usize;
        let mut remaining = qty_total;
        while remaining > 0 {
            // Peek best buy head and best sell head.
            let (bhead, bprice, shead, sprice) = {
                let book = &self.symbols[sym].book;
                let bl = book.best_level(Side::Buy);
                let sl = book.best_level(Side::Sell);
                if bl == NIL || sl == NIL {
                    break;
                }
                let b = &book.levels[bl as usize];
                let s = &book.levels[sl as usize];
                (b.head, b.price, s.head, s.price)
            };
            // Auction orders within the clearing price band only.
            if bprice < price || sprice > price {
                break;
            }
            let bo = self.symbols[sym].book.orders[bhead as usize];
            let so = self.symbols[sym].book.orders[shead as usize];
            let q = (remaining.min(bo.leaves as u64).min(so.leaves as u64)) as Qty;
            if q == 0 {
                break;
            }
            // Remove/reduce both heads.
            self.symbols[sym].book.reduce_head(Side::Buy, q);
            self.symbols[sym].book.reduce_head(Side::Sell, q);
            if bo.leaves == q {
                self.open_orders[bo.account as usize] =
                    self.open_orders[bo.account as usize].saturating_sub(1);
            }
            if so.leaves == q {
                self.open_orders[so.account as usize] =
                    self.open_orders[so.account as usize].saturating_sub(1);
            }
            // Settle at auction semantics: maker fees both sides, no aggressor.
            self.settle_auction_fill(symbol, &bo, &so, price, q, ev);
            remaining -= q as u64;
        }
    }

    /// Settlement for one auction pairing (both sides maker).
    fn settle_auction_fill(
        &mut self,
        symbol: SymbolId,
        buyer: &crate::book::Order,
        seller: &crate::book::Order,
        price: Price,
        qty: Qty,
        ev: &mut EvBuf,
    ) {
        let premium = price * qty as u64;
        let buyer_tier = self.accounts[buyer.account as usize].tier;
        let seller_tier = self.accounts[seller.account as usize].tier;
        let fees = self.cfg.fees;
        let buyer_fee = fees.maker_rate(buyer_tier) * qty as u64;
        let seller_fee = fees.maker_rate(seller_tier) * qty as u64;

        self.accounts[buyer.account as usize].cash -= premium as Cash;
        self.accounts[seller.account as usize].cash += premium as Cash;
        // Fees: both sides maker.
        for (acct, fee) in [(buyer.account, buyer_fee), (seller.account, seller_fee)] {
            if fees.maker_is_rebate {
                self.accounts[acct as usize].cash += fee as Cash;
                self.accounts[acct as usize].rebates_recv += fee;
                self.ledger.rebate_out(fee);
            } else {
                self.accounts[acct as usize].cash -= fee as Cash;
                self.accounts[acct as usize].fees_paid += fee;
                self.ledger.fee_in(fee);
            }
        }

        // Release commitments.
        let (bcc, bmc) = commit_amounts(Side::Buy, buyer.commit_pc, qty);
        self.accounts[buyer.account as usize].release(bcc, bmc);
        let (scc, smc) = commit_amounts(Side::Sell, seller.commit_pc, qty);
        self.accounts[seller.account as usize].release(scc, smc);

        // Positions.
        let buy_slot = buyer.pos_slot_of(symbol);
        let sell_slot = seller.pos_slot_of(symbol);
        self.apply_position_leg(buyer.account, buy_slot, symbol, qty as i32, price);
        self.apply_position_leg(seller.account, sell_slot, symbol, -(qty as i32), price);

        // Mark & stats.
        {
            let s = &mut self.symbols[symbol as usize];
            if s.mark != price {
                s.mark = price;
                ev.push(Event::Mark { symbol, price });
            }
            if s.open == 0 {
                s.open = price;
            }
            if price > s.high {
                s.high = price;
            }
            if s.low == 0 || price < s.low {
                s.low = price;
            }
            s.last = price;
            s.vol += qty as u64;
            s.vwap_num += premium as u128;
            s.n_trades += 1;
        }
        self.stats.trades += 1;
        self.stats.volume += qty as u64;
        self.stats.premium += premium as u128;

        ev.push(Event::Trade {
            symbol,
            price,
            qty,
            taker: 0,
            maker: 0,
            taker_acct: buyer.account,
            maker_acct: seller.account,
            aggressor: 2,
            taker_fee: 0,
            maker_fee: buyer_fee,
            maker_rebate: fees.maker_is_rebate,
        });
    }

    /// Cash-settle all positions of `symbol` at `settle`.
    fn exercise_positions(&mut self, symbol: SymbolId, settle: Price, ev: &mut EvBuf) {
        let listing = self.symbols[symbol as usize].listing;
        let intrinsic = listing.intrinsic(settle);
        let fee_pc = self.cfg.fees.exercise;
        let mut slots: Vec<u32> = Vec::new();
        for (slot, meta) in self.pos_meta.iter().enumerate() {
            if meta.symbol == symbol && self.positions[slot].qty != 0 {
                slots.push(slot as u32);
            }
        }
        for slot in slots {
            let meta = self.pos_meta[slot as usize];
            let p = self.positions[slot as usize];
            let qty_abs = p.qty.unsigned_abs();
            let flow: Cash = if intrinsic >= 1 {
                let gross = intrinsic as Cash * qty_abs as Cash;
                let fee = fee_pc * qty_abs as u64;
                if p.qty > 0 {
                    gross - fee as Cash
                } else {
                    -gross - fee as Cash
                }
            } else {
                0
            };
            let fee = if intrinsic >= 1 {
                fee_pc * qty_abs as u64
            } else {
                0
            };
            let acct = &mut self.accounts[meta.account as usize];
            acct.cash += flow;
            acct.fees_paid += fee;
            acct.margin_required = acct.margin_required.saturating_sub(p.margin);
            if let Some(i) = acct.pos.iter().position(|&(s, _)| s == symbol) {
                acct.pos.swap_remove(i);
            }
            self.acct_delta[meta.account as usize] -= p.delta_notional;
            self.positions[slot as usize] = Position::FLAT;
            if let Some(i) = self.short_slots.iter().position(|&s| s == slot) {
                self.short_slots.swap_remove(i);
            }
            self.pos_free.push(slot);
            if fee > 0 {
                self.ledger.fee_in(fee);
            }
            ev.push(Event::Exercised {
                symbol,
                account: meta.account,
                qty: qty_abs,
                intrinsic,
                fee,
                cash_flow: flow,
            });
        }
    }

    // ------------------------------------------------------------------
    // Time advance
    // ------------------------------------------------------------------

    /// Advance logical time, firing the expiry/auction wheel.
    pub fn advance(&mut self, to: Nanos, ev: &mut EvBuf) {
        if to <= self.clock {
            return;
        }
        let from_min = self.clock / NANOS_PER_MIN;
        let to_min = to / NANOS_PER_MIN;
        let mut m = from_min + 1;
        while m <= to_min {
            self.clock = m * NANOS_PER_MIN;
            let mut due = Vec::new();
            self.wheel.pop_minute(m, &mut due);
            for a in due {
                self.apply_action(a, ev);
            }
            m += 1;
        }
        self.clock = to;
        if self.halted && self.clock >= self.halt_until {
            self.resume(ev);
        }
        // Margin refresh on TTL-bucket boundaries only (>= 15 minutes of
        // elapsed logical time), or immediately on oracle moves (in
        // `oracle`). Short option margins depend on TTL buckets that change
        // at 30m/2h boundaries and the pin-zone window; 15-minute cadence
        // covers both deterministically.
        if to_min.saturating_sub(self.last_margin_min) >= 15 {
            self.last_margin_min = to_min;
            self.refresh_margins();
        }
    }

    fn apply_action(&mut self, a: Action, ev: &mut EvBuf) {
        match a {
            Action::OpenAuction(sym) => self.open_auction(sym, ev),
            Action::CloseAuction(sym) => self.close_auction(sym, ev),
            Action::Settle(sym) => self.settle_symbol(sym, ev),
        }
    }

    // ------------------------------------------------------------------
    // BBO emission
    // ------------------------------------------------------------------

    fn emit_bbo(&mut self, symbol: SymbolId, ev: &mut EvBuf) {
        let s = &self.symbols[symbol as usize];
        let bid = s.book.best_bid();
        let ask = s.book.best_ask();
        let cur = (
            bid.map(|b| b.0).unwrap_or(0),
            bid.map(|b| b.1).unwrap_or(0),
            ask.map(|a| a.0).unwrap_or(0),
            ask.map(|a| a.1).unwrap_or(0),
        );
        if cur != s.bbo {
            self.symbols[symbol as usize].bbo = cur;
            ev.push(Event::Bbo {
                symbol,
                bid: cur.0,
                bid_qty: cur.1,
                ask: cur.2,
                ask_qty: cur.3,
            });
        }
    }

    // ------------------------------------------------------------------
    // Audit & state hash
    // ------------------------------------------------------------------

    /// Conservation invariant: `sum(cash) + house == deposits`.
    pub fn conservation(&self) -> bool {
        let mut sum: Cash = 0;
        for a in &self.accounts {
            if a.id != NIL {
                sum += a.cash;
            }
        }
        sum + self.ledger.house == self.ledger.deposits
    }

    /// Full integrity audit.
    pub fn audit(&self) -> AuditReport {
        let mut sum: Cash = 0;
        let mut live = 0usize;
        let mut hold_sum: Cash = 0;
        let mut margin_sum = 0u64;
        let mut reserved_sum = 0u64;
        let mut negative = 0usize;
        for a in &self.accounts {
            if a.id == NIL {
                continue;
            }
            live += 1;
            sum += a.cash;
            hold_sum += a.hold;
            margin_sum += a.margin_required;
            reserved_sum += a.margin_reserved;
            if a.cash < 0 {
                negative += 1;
            }
        }
        let mut open_positions = 0usize;
        for p in &self.positions {
            if p.qty != 0 {
                open_positions += 1;
            }
        }
        let mut book_orders = 0u64;
        for s in &self.symbols {
            book_orders += s.book.len() as u64;
        }
        // Margin consistency: account.margin_required == sum of slot margins.
        let mut margin_ok = true;
        for a in &self.accounts {
            if a.id == NIL {
                continue;
            }
            let mut m = 0u64;
            for &(_, slot) in &a.pos {
                m += self.positions[slot as usize].margin;
            }
            if m != a.margin_required {
                margin_ok = false;
            }
        }
        AuditReport {
            conservation_ok: self.conservation(),
            sum_cash: sum,
            house: self.ledger.house,
            deposits: self.ledger.deposits,
            live_accounts: live,
            open_positions,
            symbols: self.symbols.len(),
            book_orders,
            hold_sum,
            margin_required_sum: margin_sum,
            margin_reserved_sum: reserved_sum,
            negative_cash: negative,
            margin_consistent: margin_ok,
        }
    }

    /// Deterministic state hash (FNV-1a over canonical serialization).
    /// Replaying the same command stream into a fresh shard yields the
    /// identical hash, byte for byte.
    pub fn state_hash(&self) -> u64 {
        let mut h: u64 = 0xCBF2_9CE4_8422_2325;
        let feed = |bytes: &[u8], h: &mut u64| {
            for &b in bytes {
                *h ^= b as u64;
                *h = h.wrapping_mul(0x0000_0100_0000_01B3);
            }
        };
        // Ledger & session scalars.
        feed(&self.ledger.house.to_le_bytes(), &mut h);
        feed(&self.ledger.deposits.to_le_bytes(), &mut h);
        feed(&self.ledger.fees_collected.to_le_bytes(), &mut h);
        feed(&self.ledger.rebates_paid.to_le_bytes(), &mut h);
        feed(&self.clock.to_le_bytes(), &mut h);
        feed(&self.seq.to_le_bytes(), &mut h);
        feed(&self.ref_price.to_le_bytes(), &mut h);
        feed(&self.mark_price.to_le_bytes(), &mut h);
        feed(&(self.halted as u8).to_le_bytes(), &mut h);
        feed(&(self.fast as u8).to_le_bytes(), &mut h);
        // Accounts.
        for a in &self.accounts {
            if a.id == NIL {
                continue;
            }
            feed(&a.id.to_le_bytes(), &mut h);
            feed(&(a.tier as u8).to_le_bytes(), &mut h);
            feed(&a.cash.to_le_bytes(), &mut h);
            feed(&a.hold.to_le_bytes(), &mut h);
            feed(&a.margin_required.to_le_bytes(), &mut h);
            feed(&a.margin_reserved.to_le_bytes(), &mut h);
            feed(&(a.pos.len() as u64).to_le_bytes(), &mut h);
            for &(s, slot) in &a.pos {
                feed(&s.to_le_bytes(), &mut h);
                feed(&slot.to_le_bytes(), &mut h);
                let p = self.positions[slot as usize];
                feed(&(p.qty as i64).to_le_bytes(), &mut h);
                feed(&p.margin.to_le_bytes(), &mut h);
                feed(&p.delta_notional.to_le_bytes(), &mut h);
            }
        }
        // Symbols and books.
        for s in &self.symbols {
            feed(&s.listing.key().to_le_bytes(), &mut h);
            feed(&(s.phase as u8).to_le_bytes(), &mut h);
            feed(&s.mark.to_le_bytes(), &mut h);
            feed(&s.settle.to_le_bytes(), &mut h);
            feed(&s.open.to_le_bytes(), &mut h);
            feed(&s.high.to_le_bytes(), &mut h);
            feed(&s.low.to_le_bytes(), &mut h);
            feed(&s.vol.to_le_bytes(), &mut h);
            feed(&s.vwap_num.to_le_bytes(), &mut h);
            feed(&s.n_trades.to_le_bytes(), &mut h);
            for side in [Side::Buy, Side::Sell] {
                let mut lvl = s.book.sides[side.idx()].best;
                while lvl != NIL {
                    let l = &s.book.levels[lvl as usize];
                    feed(&l.price.to_le_bytes(), &mut h);
                    feed(&l.total_qty.to_le_bytes(), &mut h);
                    feed(&l.n_orders.to_le_bytes(), &mut h);
                    let mut oid = l.head;
                    while oid != NIL {
                        let o = &s.book.orders[oid as usize];
                        feed(&o.uid.to_le_bytes(), &mut h);
                        feed(&o.account.to_le_bytes(), &mut h);
                        feed(&o.price.to_le_bytes(), &mut h);
                        feed(&o.leaves.to_le_bytes(), &mut h);
                        feed(&o.flags.to_le_bytes(), &mut h);
                        feed(&o.commit_pc.to_le_bytes(), &mut h);
                        oid = o.next;
                    }
                    lvl = l.next;
                }
            }
        }
        h
    }
}

/// One step of the match loop.
enum Step {
    /// Stop matching (no crossing level or nothing left).
    Stop,
    /// A fill of `qty` executed.
    Filled(Qty),
    /// An STP decrement of `qty` executed (no trade).
    Stp(Qty),
}

/// Commitment amounts for a (side, per-contract, qty) triple.
#[inline(always)]
fn commit_amounts(side: Side, commit_pc: u64, qty: Qty) -> (Cash, u64) {
    match side {
        Side::Buy => ((commit_pc * qty as u64) as Cash, 0u64),
        Side::Sell => (0, commit_pc * qty as u64),
    }
}

impl crate::book::Order {
    /// Position slot resolution helper for auction fills.
    fn pos_slot_of(&self, symbol: SymbolId) -> u32 {
        let _ = symbol;
        self.pos_slot
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::symbol::OptKind;

    fn base_symbol() -> Listing {
        Listing {
            underlying: 0,
            expiry_min: 960, // 16:00
            kind: OptKind::Call,
            strike: 500_000,
        }
    }

    fn setup() -> Shard {
        let mut s = Shard::new(0, 0, ShardConfig::default_config());
        s.execute(
            Cmd::CreateAccount {
                account: 1,
                tier: Tier::Base,
            },
            &mut EvBuf::new(),
        );
        s.execute(
            Cmd::CreateAccount {
                account: 2,
                tier: Tier::Base,
            },
            &mut EvBuf::new(),
        );
        s.execute(
            Cmd::CreateSymbol {
                listing: base_symbol(),
            },
            &mut EvBuf::new(),
        );
        s.execute(
            Cmd::Deposit {
                account: 1,
                amount: 10_000_000,
            },
            &mut EvBuf::new(),
        );
        s.execute(
            Cmd::Deposit {
                account: 2,
                amount: 10_000_000,
            },
            &mut EvBuf::new(),
        );
        s.execute(
            Cmd::Oracle { price: 500_000 },
            &mut EvBuf::new(),
        );
        s
    }

    fn order(sym: u32, acct: u32, side: Side, price: Price, qty: Qty) -> NewOrder {
        NewOrder {
            symbol: sym,
            account: acct,
            side,
            price,
            qty,
            tif: Tif::Gtc,
            kind: Kind::Limit,
            flags: 0,
            oco: NO_ID,
        }
    }

    #[test]
    fn rest_and_cross_full_cycle() {
        let mut s = setup();
        let mut ev = EvBuf::new();
        // Maker sell 5 @ 5.00.
        s.execute(Cmd::Place(order(0, 2, Side::Sell, 500, 5)), &mut ev);
        // Taker buy 5 @ 5.00 crosses fully.
        s.execute(Cmd::Place(order(0, 1, Side::Buy, 500, 5)), &mut ev);
        assert!(s.conservation());
        let a1 = &s.accounts[1];
        let a2 = &s.accounts[2];
        // Buyer paid 2500 premium + 325 taker fee.
        assert_eq!(a1.cash, 10_000_000 - 2_500 - 325);
        // Seller received 2500 premium + 50 maker rebate.
        assert_eq!(a2.cash, 10_000_000 + 2_500 + 50);
        // Positions: buyer long 5, seller short 5.
        assert_eq!(s.positions[a1.pos_slot(0).unwrap() as usize].qty, 5);
        assert_eq!(s.positions[a2.pos_slot(0).unwrap() as usize].qty, -5);
        // House: 325 fee in, 50 rebate out.
        assert_eq!(s.ledger.house, 275);
        assert_eq!(s.stats.trades, 1);
        assert_eq!(s.symbols[0].mark, 500);
    }

    #[test]
    fn partial_fill_then_rest() {
        let mut s = setup();
        let mut ev = EvBuf::new();
        s.execute(Cmd::Place(order(0, 2, Side::Sell, 500, 10)), &mut ev);
        s.execute(Cmd::Place(order(0, 1, Side::Buy, 500, 4)), &mut ev);
        assert_eq!(s.symbols[0].book.best_ask(), Some((500, 6)));
        let a1 = &s.accounts[1];
        assert_eq!(a1.cash, 10_000_000 - 2_000 - 260);
        assert_eq!(a1.hold, 0);
        assert_eq!(s.symbols[0].mark, 500);
    }

    #[test]
    fn insufficient_funds_rejected() {
        let mut s = setup();
        let mut ev = EvBuf::new();
        // Expensive buy beyond available.
        s.execute(Cmd::Place(order(0, 1, Side::Buy, 2_000_000, 10)), &mut ev);
        let rejected = ev.events.iter().any(|e| matches!(
            e,
            Event::Reject { code: 5, .. }
        ));
        assert!(rejected);
        assert_eq!(s.stats.orders, 0);
    }

    #[test]
    fn post_only_rejects_crossing() {
        let mut s = setup();
        let mut ev = EvBuf::new();
        s.execute(Cmd::Place(order(0, 2, Side::Sell, 500, 5)), &mut ev);
        let mut po = order(0, 1, Side::Buy, 500, 5);
        po.flags |= flags::POST_ONLY;
        s.execute(Cmd::Place(po), &mut ev);
        assert!(ev
            .events
            .iter()
            .any(|e| matches!(e, Event::Reject { code: 3, .. })));
    }

    #[test]
    fn stp_decrement_and_cancel() {
        let mut s = setup();
        let mut ev = EvBuf::new();
        // Account 1 rests both sides at crossing prices.
        s.execute(Cmd::Place(order(0, 1, Side::Sell, 500, 5)), &mut ev);
        s.execute(Cmd::Place(order(0, 1, Side::Buy, 500, 3)), &mut ev);
        // No trade, maker sell order cancelled, no cash moved by a trade.
        assert_eq!(s.stats.trades, 0);
        assert_eq!(s.symbols[0].book.best_ask(), None);
        let a1 = &s.accounts[1];
        assert_eq!(a1.cash, 10_000_000);
        assert_eq!(a1.hold, 0); // taker released its hold for the STP qty
        assert!(s.conservation());
    }

    #[test]
    fn fok_and_ioc() {
        let mut s = setup();
        let mut ev = EvBuf::new();
        s.execute(Cmd::Place(order(0, 2, Side::Sell, 500, 3)), &mut ev);
        // FOK for 5: only 3 available -> reject.
        let mut fok = order(0, 1, Side::Buy, 500, 5);
        fok.tif = Tif::Fok;
        s.execute(Cmd::Place(fok), &mut ev);
        assert!(ev
            .events
            .iter()
            .any(|e| matches!(e, Event::Reject { code: 10, .. })));
        // IOC for 5: fills 3, cancels rest.
        let mut ioc = order(0, 1, Side::Buy, 500, 5);
        ioc.tif = Tif::Ioc;
        s.execute(Cmd::Place(ioc), &mut ev);
        assert_eq!(s.stats.trades, 1);
        assert_eq!(s.positions[s.accounts[1].pos_slot(0).unwrap() as usize].qty, 3);
        assert_eq!(s.symbols[0].book.len(), 0);
    }

    #[test]
    fn expiry_settles_intrinsic_and_cleans_up() {
        let mut s = setup();
        let mut ev = EvBuf::new();
        // Trade 5 @ 5.00: acct1 long, acct2 short.
        s.execute(Cmd::Place(order(0, 2, Side::Sell, 500, 5)), &mut ev);
        s.execute(Cmd::Place(order(0, 1, Side::Buy, 500, 5)), &mut ev);
        // Underlying rallies to 505.00 -> intrinsic 500 ticks/contract.
        s.execute(Cmd::Oracle { price: 505_000 }, &mut ev);
        // Advance past 16:00 expiry.
        s.execute(
            Cmd::Advance {
                to: 961 * NANOS_PER_MIN,
            },
            &mut ev,
        );
        assert_eq!(s.symbols[0].phase, Phase::Expired);
        assert_eq!(s.symbols[0].settle, 505_000);
        assert!(s.conservation());
        // Long receives 5 * intrinsic(5,000 ticks) = 25,000 minus exercise
        // fees 5*10 = 50 (premium paid 2,500 + taker fee 325 earlier).
        assert_eq!(
            s.accounts[1].cash,
            10_000_000 - 2_500 - 325 + 25_000 - 50
        );
        // Short pays 25,000 intrinsic + 50 fees (kept 2,500 premium + 50 rebate).
        assert_eq!(
            s.accounts[2].cash,
            10_000_000 + 2_500 + 50 - 25_000 - 50
        );
        // Positions all flat and released.
        assert_eq!(s.audit().open_positions, 0);
        assert_eq!(s.accounts[1].margin_required, 0);
        assert_eq!(s.accounts[1].pos.len(), 0);
    }

    #[test]
    fn short_margin_grows_and_blocks() {
        let mut s = setup();
        let mut ev = EvBuf::new();
        // A monster quantity order is stopped by the fat-finger cap first.
        s.execute(Cmd::Place(order(0, 2, Side::Sell, 500, 100_000)), &mut ev);
        assert!(ev
            .events
            .iter()
            .any(|e| matches!(e, Event::Reject { code: 16, .. })));
        // At the position limit, the margin bill exceeds cash (10M).
        s.execute(Cmd::Place(order(0, 2, Side::Sell, 500, 5_000)), &mut ev);
        assert!(ev
            .events
            .iter()
            .any(|e| matches!(e, Event::Reject { code: 5, .. })));
        // Reasonable short is accepted and margined.
        s.execute(Cmd::Place(order(0, 2, Side::Sell, 500, 100)), &mut ev);
        let a2 = &s.accounts[2];
        assert!(a2.margin_reserved > 0);
        assert!(a2.available() < 10_000_000);
        assert!(s.conservation());
    }

    #[test]
    fn hardening_caps_and_ownership() {
        let mut s = setup();
        let mut ev = EvBuf::new();
        // Book flood: resting orders stop at max_open_orders per account.
        let mut rejects_too_many = 0;
        for i in 0..300u64 {
            ev.clear();
            s.execute(
                Cmd::Place(order(0, 2, Side::Sell, 300 + i % 200, 1)),
                &mut ev,
            );
            if ev
                .events
                .iter()
                .any(|e| matches!(e, Event::Reject { code: 15, .. }))
            {
                rejects_too_many += 1;
            }
        }
        assert!(rejects_too_many > 0, "book-flood cap never tripped");
        assert!(s.conservation());
        // Find one of account 2's resting orders and try to cancel as
        // account 1 -> NotOwner.
        let mut found = None;
        for (i, o) in s.symbols[0].book.orders.iter().enumerate() {
            if o.account == 2 && o.level != crate::book::NIL {
                found = Some(crate::types::pack_id(i as u32, o.uid));
            }
        }
        let oid = found.expect("account 2 has resting orders");
        ev.clear();
        s.execute(
            Cmd::Cancel {
                order: oid,
                symbol: 0,
                account: 1,
            },
            &mut ev,
        );
        assert!(ev
            .events
            .iter()
            .any(|e| matches!(e, Event::Reject { code: 17, .. })));
        // Owner cancels fine.
        ev.clear();
        s.execute(
            Cmd::Cancel {
                order: oid,
                symbol: 0,
                account: 2,
            },
            &mut ev,
        );
        assert!(ev
            .events
            .iter()
            .any(|e| matches!(e, Event::Canceled { reason: 0, .. })));
        // Negative deposit is ignored (defense in depth for admin APIs).
        ev.clear();
        s.execute(
            Cmd::Deposit {
                account: 1,
                amount: -5_000,
            },
            &mut ev,
        );
        assert!(ev.events.is_empty());
        assert_eq!(s.accounts[1].cash, 10_000_000);
        assert!(s.conservation());
    }

    #[test]
    fn state_hash_deterministic_across_replay() {
        let mut s = setup();
        let mut ev = EvBuf::new();
        s.execute(Cmd::Place(order(0, 2, Side::Sell, 500, 10)), &mut ev);
        s.execute(Cmd::Place(order(0, 1, Side::Buy, 500, 4)), &mut ev);
        s.execute(Cmd::Place(order(0, 2, Side::Sell, 520, 6)), &mut ev);
        s.execute(Cmd::Oracle { price: 501_000 }, &mut ev);
        let h1 = s.state_hash();
        // Same command stream into a fresh shard (ignore the different
        // journal, compare engine state).
        let cmds = vec![
            Cmd::CreateAccount {
                account: 1,
                tier: Tier::Base,
            },
            Cmd::CreateAccount {
                account: 2,
                tier: Tier::Base,
            },
            Cmd::CreateSymbol {
                listing: base_symbol(),
            },
            Cmd::Deposit {
                account: 1,
                amount: 10_000_000,
            },
            Cmd::Deposit {
                account: 2,
                amount: 10_000_000,
            },
            Cmd::Oracle { price: 500_000 },
            Cmd::Place(order(0, 2, Side::Sell, 500, 10)),
            Cmd::Place(order(0, 1, Side::Buy, 500, 4)),
            Cmd::Place(order(0, 2, Side::Sell, 520, 6)),
            Cmd::Oracle { price: 501_000 },
        ];
        let mut s2 = Shard::new(0, 0, ShardConfig::default_config());
        s2.journaling = false;
        for c in cmds {
            s2.execute(c, &mut EvBuf::new());
        }
        assert_eq!(h1, s2.state_hash());
    }

    #[test]
    fn circuit_breaker_halts_and_reopens() {
        let mut s = setup();
        let mut ev = EvBuf::new();
        s.execute(Cmd::Place(order(0, 2, Side::Sell, 500, 5)), &mut ev);
        // 8% gap down triggers the breaker.
        s.execute(Cmd::Oracle { price: 460_000 }, &mut ev);
        assert!(s.halted);
        // Orders rejected while halted.
        s.execute(Cmd::Place(order(0, 1, Side::Buy, 500, 1)), &mut ev);
        assert!(ev
            .events
            .iter()
            .any(|e| matches!(e, Event::Reject { code: 13, .. })));
        // Advance past halt -> reopening auction -> continuous.
        s.execute(
            Cmd::Advance {
                to: 6 * NANOS_PER_MIN,
            },
            &mut ev,
        );
        assert!(!s.halted);
        assert_eq!(s.symbols[0].phase, Phase::AuctionOpen);
        s.execute(
            Cmd::Advance {
                to: 8 * NANOS_PER_MIN,
            },
            &mut ev,
        );
        assert_eq!(s.symbols[0].phase, Phase::Continuous);
        // The resting sell survives the reopen.
        assert_eq!(s.symbols[0].book.best_ask(), Some((500, 5)));
    }

    #[test]
    fn moc_auction_clears_at_single_price() {
        let mut s = setup();
        let mut ev = EvBuf::new();
        // Auction interest accumulates during the window.
        s.execute(
            Cmd::Advance {
                to: 950 * NANOS_PER_MIN,
            },
            &mut ev,
        ); // 15:50 -> auction opens
        assert_eq!(s.symbols[0].phase, Phase::AuctionOpen);
        s.execute(Cmd::Place(order(0, 1, Side::Buy, 700, 10)), &mut ev);
        s.execute(Cmd::Place(order(0, 2, Side::Sell, 400, 6)), &mut ev);
        // Settle at 16:00.
        s.execute(
            Cmd::Advance {
                to: 961 * NANOS_PER_MIN,
            },
            &mut ev,
        );
        assert_eq!(s.symbols[0].phase, Phase::Expired);
        // Clearing: both 700 and 400 match 6 lots; reference = midpoint of
        // the crossed book (550) -> tie -> lower price wins: 400.
        assert_eq!(s.symbols[0].settle, 500_000); // underlying mark = strike
        assert!(s.conservation());
        // Buyer: premium 6*400 = 2400, maker rebate 6*10 = 60.
        // Underlying settles at strike -> intrinsic 0 -> expires worthless.
        let a1 = &s.accounts[1];
        assert_eq!(a1.cash, 10_000_000 - 2_400 + 60);
        let a2 = &s.accounts[2];
        assert_eq!(a2.cash, 10_000_000 + 2_400 + 60);
        assert_eq!(s.stats.trades, 1);
        assert_eq!(s.stats.auctions, 1);
    }

    #[test]
    fn oco_cascade() {
        let mut s = setup();
        let mut ev = EvBuf::new();
        let first = Cmd::Place(order(0, 1, Side::Buy, 400, 5));
        s.execute(first, &mut ev);
        let ack_id = ev
            .events
            .iter()
            .rev()
            .find_map(|e| match e {
                Event::Ack { order, .. } => Some(*order),
                _ => None,
            })
            .unwrap();
        // OCO sell above.
        let mut oco = order(0, 1, Side::Sell, 600, 5);
        oco.oco = ack_id;
        s.execute(Cmd::Place(oco), &mut ev);
        // Cancel the buy; the sell must cascade.
        s.execute(
            Cmd::Cancel {
                order: ack_id,
                symbol: 0,
                account: 1,
            },
            &mut ev,
        );
        assert_eq!(s.symbols[0].book.len(), 0);
        assert_eq!(s.accounts[1].hold, 0);
        assert_eq!(s.accounts[1].margin_reserved, 0);
    }
}
