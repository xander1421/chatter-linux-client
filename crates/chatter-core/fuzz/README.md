# chatter-core fuzz harnesses

libFuzzer-based fuzz targets for crypto parsers in `chatter-core`.

This crate is **excluded** from the main workspace (see root `Cargo.toml`'s
`[workspace] exclude`) so `libfuzzer-sys` is not pulled into normal builds.

## Targets

- `fuzz_envelope_parse` — feeds random byte slices into
  `chatter_core::crypto::decrypt_envelope`. Property: never panics; AEAD
  rejection on random bytes is the expected outcome.
- `fuzz_sealed_sender_cert` — feeds random byte slices into
  `SenderCertificate::deserialize` and (if it parses) into
  `validate_trust_root`. Property: never panics; never validates against the
  hardcoded trust root.

## Running

```bash
cargo install cargo-fuzz
cd crates/chatter-core/fuzz
cargo fuzz run fuzz_envelope_parse        -- -max_total_time=60
cargo fuzz run fuzz_sealed_sender_cert    -- -max_total_time=60
```

`cargo-fuzz` requires a nightly toolchain (`rustup toolchain install nightly`).

For longer runs, drop `-max_total_time` or raise it; corpus files land in
`corpus/<target>/`, crash reproducers in `artifacts/<target>/`.

## CI / quick compile-check

To verify the harness compiles without actually fuzzing:

```bash
cd crates/chatter-core/fuzz
cargo check
```
