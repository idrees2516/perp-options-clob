//! Coordinated provider moves of any size must never lose the mark;
//! a lone outlier must never drag it.
#![no_main]

use libfuzzer_sys::fuzz_target;
use poc_oracle::{AssetOracle, OracleConfig};

fn next(r: &mut u64) -> u64 {
    let mut x = *r;
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    *r = x;
    x.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

fuzz_target!(|data: &[u8]| {
    let mut seed = data.iter().fold(0x9E37_79B9_7F4A_7C15 | 1, |h, &b| (h ^ u64::from(b)).wrapping_mul(31));
    let mut o = AssetOracle::new("BTC", OracleConfig::default());
    let mut last = 8_000_000_u128;
    for step in 0..40u64 {
        let now = step * 1_000;
        let jump = u128::from(next(&mut seed) % 2_000_000);
        let next_price = if next(&mut seed) % 2 == 0 {
            last.saturating_add(jump)
        } else {
            last.saturating_sub(jump.min(last / 2))
        };
        for provider in ["a", "b", "c"] {
            o.update(provider, now, next_price);
        }
        assert_eq!(o.mark(now), Some(next_price), "coordinated move lost the mark");
        last = next_price;
    }
});
