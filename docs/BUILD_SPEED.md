# Development build speed

## Profiles

`[profile.dev]` in the workspace `Cargo.toml` builds workspace crates at
`opt-level = 0`, `debug = 0`, `incremental = true`, `codegen-units = 256`.
Dependencies (`[profile.dev.package."*"]`) build at `opt-level = 2`. An explicit
`CARGO_INCREMENTAL` in the environment overrides the profile.

Two per-package exceptions, both explained in `Cargo.toml` comments:

- `chatter-ui-slint` is non-incremental (`incremental = false`). Its incremental
  memory use was erratic and sccache refuses incremental builds.
- `chatter-attachment-crypto` builds at `opt-level = 2`, so the attachment
  AES-CBC/HMAC code instantiated there is not compiled at O0. `chatter-core`
  builds at O0 like every other member.

Incremental compilation helps rebuilds after an edit, not the first build.
`.cargo/config.toml` sets `rustc-wrapper = "sccache"` and, on
`x86_64-unknown-linux-gnu`, links with clang + mold.

## Opt-in Slint live preview

Run `tools/ui-live-preview.sh` with your usual development features and public
build trust configuration. For example, if the local development anchors and
server are configured:

```sh
tools/ui-live-preview.sh --features mesh,dev-trust-root,dev-auto-register
```

The script sets `SLINT_LIVE_PREVIEW=1`, `CHATTER_NO_KEYRING=1` and a separate
`XDG_DATA_HOME` (default `~/.cache/chatter-ui-preview/data`), then runs
`cargo run -p chatter-ui --bin chatter-ui --features slint/live-preview`
(see [Slint's live-preview guide](https://docs.slint.dev/latest/docs/slint/guide/tooling/live-preview/)).
Preview is not a default feature. It does not copy existing accounts or
credentials. Auto-registration additionally requires the `CHATTER_DEV=1`
opt-in; use only with the local development server.

The initial build still costs time. Subsequent `.slint` edits can reload in the
running app; changes to properties/callbacks used by Rust require a rebuild.
Compatibility with this repository's patched Backdrop item and renderer in the
interpreter is unverified. Never package the preview binary.

## Test binaries

Do not blanket-merge the UI integration suites. Several initialize Slint's
process-wide event-loop backend and intentionally have one test per executable.
The [backend documentation](https://docs.slint.dev/latest/docs/rust/i_slint_backend_testing/fn.init_integration_test_with_system_time)
requires one initialization per process. Other suites mutate environment variables
or include application modules with crate-root paths. Consolidation needs an
explicit process-isolating runner and a review of those assumptions first.

## Avoid invalidating or blocking caches

Keep compiler flags, feature sets and target CPU stable between edits: changing
`RUSTFLAGS` (for example the target CPU) rebuilds every dependency. Platforms
sharing one target directory wait on Cargo's target-directory lock, so build them
sequentially. Separate target directories avoid the lock but duplicate large
artifacts and do not create more RAM. Do not clean an active build directory.
