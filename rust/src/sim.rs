//! Deterministic market simulator: agents, price paths, session lifecycle.
//!
//! One `xorshift64*` stream drives everything, so a (seed, schedule) pair
//! reproduces a trading day exactly. Used by the E2E day test, the
//! randomized invariant suite and the TPS harness (self-driving mode).

use crate::engine::{Cmd, Shard, NANOS_PER_MIN};
use crate::fees::Tier;
use crate::journal::EvBuf;
use crate::symbol::{Listing, OptKind};
use crate::types::*;

/// Deterministic PRNG (xorshift64*).
#[derive(Clone, Copy)]
pub struct Rng(pub u64);

#[allow(clippy::should_implement_trait)]
impl Rng {
    /// New stream (any non-zero seed).
    pub fn new(seed: u64) -> Rng {
        Rng(seed | 1)
    }
    /// Next raw value.
    #[inline(always)]
    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    /// Uniform below `n`.
    #[inline(always)]
    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    /// Uniform in [lo, hi] ticks.
    #[inline(always)]
    pub fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.below(hi - lo + 1)
    }
    /// True with probability num/den.
    #[inline(always)]
    pub fn chance(&mut self, num: u64, den: u64) -> bool {
        self.below(den) < num
    }
    /// Random side.
    #[inline(always)]
    pub fn side(&mut self) -> Side {
        if self.next() & 1 == 0 {
            Side::Buy
        } else {
            Side::Sell
        }
    }
}

/// Simulated trading-day schedule.
#[derive(Clone, Copy)]
pub struct DaySchedule {
    /// Session open (nanos).
    pub open: Nanos,
    /// Session close (nanos).
    pub close: Nanos,
    /// Expiry minutes (minute-of-day) for the chain.
    pub expiries: [u32; 7],
    /// Strike ladder (ticks).
    pub strikes: [u64; 8],
}

impl DaySchedule {
    /// A standard 0DTE index day: 9:30 open, hourly expiries, 50-point strikes.
    pub const fn default_day() -> DaySchedule {
        DaySchedule {
            open: 9 * 3_600_000_000_000 + 30 * NANOS_PER_MIN,
            close: 16 * 3_600_000_000_000 + 15 * NANOS_PER_MIN,
            expiries: [600, 660, 720, 780, 840, 900, 960],
            strikes: [
                480_000, 490_000, 495_000, 500_000, 505_000, 510_000, 520_000, 530_000,
            ],
        }
    }
}

/// One synthetic agent's intent for the next command.
#[derive(Clone, Copy, Debug)]
pub enum AgentCmd {
    /// Place an order.
    Place(NewOrder),
    /// Cancel a resting order (owner, order, symbol).
    Cancel(AcctId, OrderId, SymbolId),
    /// Advance time by one step.
    Tick,
    /// New underlying mark.
    Mark(Price),
}

/// A simple two-sided market maker + taker flow generator over one shard.
pub struct Flow {
    /// PRNG stream.
    pub rng: Rng,
    /// Trading accounts (id, tier).
    pub accounts: Vec<(AcctId, Tier)>,
    /// Underlying reference at start.
    pub spot0: Price,
    /// Current spot.
    pub spot: Price,
    /// Volatility per tick (basis points of spot).
    pub vol_bps: u64,
    /// Symbols registered on the shard (local ids).
    pub symbols: Vec<SymbolId>,
    /// Resting order ids per account for cancels.
    pub resting: Vec<Vec<(OrderId, SymbolId)>>,
    /// Symbol of the most recent generated Place (for ack bookkeeping).
    pub pending_symbol: SymbolId,
    /// Every command executed during boot (for exact replay).
    pub boot_cmds: Vec<Cmd>,
    /// Number of generated commands.
    pub generated: u64,
    /// Step counter.
    pub step: u64,
}

impl Flow {
    /// Wire up accounts + symbol ladder on a shard and open the session.
    /// Every boot command is recorded in `boot_cmds` for exact replay.
    pub fn boot(shard: &mut Shard, seed: u64, n_accounts: usize, sched: &DaySchedule) -> Flow {
        let rng = Rng::new(seed);
        let mut accounts = Vec::new();
        let mut boot_cmds = Vec::new();
        let day_min = sched.open / NANOS_PER_MIN;
        // Register accounts and fund them.
        for i in 0..n_accounts as u32 {
            let tier = if i % 5 == 0 {
                Tier::Prime
            } else if i % 3 == 0 {
                Tier::Gold
            } else {
                Tier::Base
            };
            let mut ev = EvBuf::new();
            let c1 = Cmd::CreateAccount { account: i, tier };
            shard.execute(c1, &mut ev);
            boot_cmds.push(c1);
            let c2 = Cmd::Deposit {
                account: i,
                amount: 50_000_000,
            };
            shard.execute(c2, &mut ev);
            boot_cmds.push(c2);
            accounts.push((i, tier));
        }
        // List the chain: every (expiry, strike, kind).
        let mut symbols = Vec::new();
        for &exp in sched.expiries.iter() {
            for &k in sched.strikes.iter() {
                for kind in [OptKind::Call, OptKind::Put] {
                    let listing = Listing {
                        underlying: shard.underlying,
                        expiry_min: (day_min / 1440 * 1440 + exp as u64) as u32,
                        kind,
                        strike: k,
                    };
                    let mut ev = EvBuf::new();
                    let c = Cmd::CreateSymbol { listing };
                    shard.execute(c, &mut ev);
                    boot_cmds.push(c);
                    let id = shard.symbol_id(listing);
                    symbols.push(id);
                }
            }
        }
        let spot0 = 500_000;
        let mut ev = EvBuf::new();
        let ca = Cmd::Advance {
            to: sched.open - NANOS_PER_MIN,
        };
        shard.execute(ca, &mut ev);
        boot_cmds.push(ca);
        let co = Cmd::Oracle { price: spot0 };
        shard.execute(co, &mut ev);
        boot_cmds.push(co);
        Flow {
            rng,
            accounts,
            spot0,
            spot: spot0,
            vol_bps: 8,
            symbols,
            resting: vec![Vec::new(); n_accounts],
            pending_symbol: 0,
            boot_cmds,
            generated: 0,
            step: 0,
        }
    }

    /// Generate the next synthetic command.
    pub fn next_cmd(&mut self) -> AgentCmd {
        self.generated += 1;
        self.step += 1;
        // Every 97 steps: market tick (mark + time).
        if self.step % 97 == 0 {
            let bump = self.rng.range(0, 2 * self.vol_bps) as i64 - self.vol_bps as i64;
            let s = self.spot as i64 + self.spot as i64 * bump / 10_000;
            self.spot = s.max(10_000) as Price;
            return AgentCmd::Mark(self.spot);
        }
        if self.step % 131 == 0 {
            return AgentCmd::Tick;
        }
        // Mostly order flow.
        let ai = self.rng.below(self.accounts.len() as u64) as usize;
        let (acct, _tier) = self.accounts[ai];
        let sym = self.symbols[self.rng.below(self.symbols.len() as u64) as usize];
        // Cancel 20% of the time when we have resting orders.
        if !self.resting[ai].is_empty() && self.rng.chance(1, 5) {
            let idx = self.rng.below(self.resting[ai].len() as u64) as usize;
            let (oid, osym) = self.resting[ai].swap_remove(idx);
            return AgentCmd::Cancel(acct, oid, osym);
        }
        // Quote around a synthetic fair value = max(intrinsic-ish, 1 tick).
        let side = self.rng.side();
        let px = self.rng.range(100, 5_000);
        let qty = self.rng.range(1, 20) as Qty;
        let tif = if self.rng.chance(1, 8) {
            Tif::Ioc
        } else {
            Tif::Gtc
        };
        let mut flags = 0u8;
        if self.rng.chance(1, 4) {
            flags |= crate::types::flags::POST_ONLY;
        }
        self.pending_symbol = sym;
        AgentCmd::Place(NewOrder {
            symbol: sym,
            account: acct,
            side,
            price: px,
            qty,
            tif,
            kind: Kind::Limit,
            flags,
            oco: NO_ID,
        })
    }

    /// Track a resting order from an Ack event (uses the pending symbol).
    pub fn note_ack(&mut self, ev: &EvBuf) {
        for e in &ev.events {
            if let crate::journal::Event::Ack {
                order, account, symbol, leaves, ..
            } = *e
            {
                if order != 0 && leaves > 0 && (account as usize) < self.resting.len() {
                    self.resting[account as usize].push((order, symbol));
                }
            }
        }
    }
}
