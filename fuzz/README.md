# Fuzz targets (nightly + libFuzzer)

These harnesses mutate bytes into structured inputs and drive the same
invariant checkers as the stable property suite
(`crates/engine/tests/properties.rs`); see `docs/FUZZING.md` for the
strategy and the three defects the program has already found.

```bash
rustup install nightly
cargo +nightly install cargo-fuzz
cargo +nightly fuzz run fuzz_matching -- -max_total_time=300
```

Corpora live in `corpus/<target>/`.
