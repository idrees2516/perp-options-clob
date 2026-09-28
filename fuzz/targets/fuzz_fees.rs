//! Option fee caps: fee never exceeds the premium cap fraction.
#![no_main]

use libfuzzer_sys::fuzz_target;
use poc_economics::{FeeCalculator, FeeSchedule, OptionFeeCaps};

fn next(r: &mut u64) -> u64 {
    let mut x = *r;
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    *r = x;
    x.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

fuzz_target!(|data: &[u8]| {
    let mut seed = data
        .iter()
        .fold(0x5DEE_CE66 | 1, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01B3));
    let caps = OptionFeeCaps::default();
    let schedule = FeeSchedule::default();
    let tier = schedule.tier_for(0).clone();
    for _ in 0..32 {
        let notional = u128::from(next(&mut seed) % 1_000_000_000);
        let premium = 1 + next(&mut seed) % 100_000;
        let fee = FeeCalculator::option_taker_fee(&tier, &caps, notional, premium).unwrap_or(0);
        let cap = poc_core::mul_div(
            premium,
            u128::from(caps.taker_cap_bps_of_premium),
            10_000,
            poc_core::Rounding::Ceil,
        )
        .unwrap_or(u128::MAX);
        assert!(
            fee.unsigned_abs() <= cap,
            "fee {fee} exceeds cap {cap}"
        );
    }
});
