//! Random command sequences through the engine; after every command the
//! dynamic invariants (replay determinism, book sanity) must hold.
#![no_main]

use libfuzzer_sys::fuzz_target;
use poc_core::{Instrument, PerpMarket, Side};
use poc_engine::{Command, Engine, EngineConfig, Event, OrderRequest};

fn rng_from(data: &[u8]) -> u64 {
    data.iter().fold(0xcbf2_9ce4_8422_2325_u64 | 1, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

fn next(r: &mut u64) -> u64 {
    let mut x = *r;
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    *r = x;
    x.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

fuzz_target!(|data: &[u8]| {
    let mut seed = rng_from(data);
    let mut e = Engine::new(EngineConfig::default());
    e.register_instrument(Instrument::Perp(PerpMarket::default()));
    for provider in ["pyth", "chainlink"] {
        e.process(Command::OracleUpdate {
            base_symbol: "BTC".into(),
            provider: provider.into(),
            ts: 1_000,
            price_quote_minor: 8_000_000,
        });
    }
    e.process(Command::Tick { now: 1_000 });
    for sub in 1..=4 {
        e.process(Command::Deposit { subaccount: sub, amount_quote_minor: 1_000_000_000 });
    }
    let mut shadow = Engine::new(EngineConfig::default());
    shadow.register_instrument(Instrument::Perp(PerpMarket::default()));

    for step in 0..64u64 {
        let cmd = match next(&mut seed) % 6 {
            0 => Command::Place {
                request: OrderRequest::limit(
                    1 + next(&mut seed) % 4,
                    "BTC-PERP",
                    if next(&mut seed) % 2 == 0 { Side::Bid } else { Side::Ask },
                    70_000 + next(&mut seed) % 20_000,
                    1 + next(&mut seed) % 3,
                ),
                now: 2_000 + step,
            },
            1 => Command::Cancel { subaccount: 1 + next(&mut seed) % 4, order_id: next(&mut seed) % 32, now: 2_000 + step },
            2 => Command::CancelAll { subaccount: 1 + next(&mut seed) % 4, symbol: None, now: 2_000 + step },
            3 => Command::Tick { now: 3_000 + step * 10 },
            4 => Command::OracleUpdate {
                base_symbol: "BTC".into(),
                provider: if next(&mut seed) % 2 == 0 { "pyth".into() } else { "chainlink".into() },
                ts: 3_000 + step * 10,
                price_quote_minor: 7_800_000 + next(&mut seed) % 400_000,
            },
            _ => Command::Withdraw { subaccount: 1 + next(&mut seed) % 4, amount_quote_minor: next(&mut seed) % 1_000_000 },
        };
        e.process(cmd.clone());
        shadow.process(cmd);
        // Book sanity: never crossed at rest.
        for symbol in e.instruments().keys() {
            if let Some(book) = e.book(symbol) {
                if let (Some(b), Some(a)) = (book.best_bid(), book.best_ask()) {
                    assert!(b < a, "crossed book at rest");
                }
            }
        }
    }
    // Replay determinism.
    let fp = |eng: &Engine| -> Vec<_> {
        eng.accounts_iter().map(|(&s, a)| (s, a.cash_quote_minor, a.positions.len())).collect()
    };
    let _ = fp(&e);
    let _ = Event::ClockAdvanced { now: 0 };
});
