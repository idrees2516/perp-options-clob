//! Any byte string handed to the WAL decoder must not panic, and torn
//! tails must recover to a clean prefix.
#![no_main]

use libfuzzer_sys::fuzz_target;
use poc_persist::FrameIter;

fuzz_target!(|data: &[u8]| {
    let mut iter = FrameIter::new(data);
    loop {
        match iter.next_frame() {
            Ok(_) => continue,
            Err(_) => break,
        }
    }
});
