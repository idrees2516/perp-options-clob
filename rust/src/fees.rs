//! Fee schedule: per-contract maker/taker fees with volume tiers, plus
//! expiry settlement fees. All values are integer ticks per contract; tier
//! multipliers are fixed-point (x/1024) so scaling is one multiply and one
//! shift on the hot path.

/// Fee tier of an account (assigned by the platform, drives multipliers).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Tier {
    /// Retail default.
    Base = 0,
    /// Moderate monthly volume.
    Silver = 1,
    /// High monthly volume.
    Gold = 2,
    /// Prop/market-maker program.
    Prime = 3,
}

/// Per-contract fee rates in ticks (1 tick = 0.01, so 65 ticks = $0.65).
#[derive(Clone, Copy, Debug)]
pub struct FeeSchedule {
    /// Taker fee per contract, base tier.
    pub taker: u64,
    /// Maker fee per contract, base tier (may be a rebate: unsigned credit).
    pub maker: u64,
    /// Maker fees are credits (rebated to the maker) rather than charges.
    pub maker_is_rebate: bool,
    /// Per-contract fee charged at expiry settlement.
    pub settle: u64,
    /// Per-contract fee charged on exercise/assignment.
    pub exercise: u64,
    /// Tier multipliers in 1024ths (index = tier).
    pub tier_mul: [u64; 4],
}

impl FeeSchedule {
    /// SPX-0DTE-flavoured default schedule.
    pub const fn default_schedule() -> FeeSchedule {
        FeeSchedule {
            taker: 65,
            maker: 10,
            maker_is_rebate: true,
            settle: 10,
            exercise: 10,
            tier_mul: [1024, 896, 768, 640],
        }
    }

    /// Effective per-contract taker fee for a tier.
    #[inline(always)]
    pub fn taker_rate(&self, tier: Tier) -> u64 {
        (self.taker * self.tier_mul[tier as usize]) >> 10
    }

    /// Effective per-contract maker fee (or rebate) for a tier.
    #[inline(always)]
    pub fn maker_rate(&self, tier: Tier) -> u64 {
        (self.maker * self.tier_mul[tier as usize]) >> 10
    }

    /// Total fee for `qty` contracts at `rate` per contract.
    #[inline(always)]
    pub fn assess(&self, rate: u64, qty: u32) -> u64 {
        rate * qty as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_scaling() {
        let f = FeeSchedule::default_schedule();
        assert_eq!(f.taker_rate(Tier::Base), 65);
        assert_eq!(f.taker_rate(Tier::Prime), (65 * 640) >> 10);
        assert_eq!(f.maker_rate(Tier::Prime), (10 * 640) >> 10);
        assert_eq!(f.assess(f.taker_rate(Tier::Base), 100), 6_500);
    }
}
