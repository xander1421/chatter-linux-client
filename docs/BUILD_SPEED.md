# Development build speed

The workspace enables incremental compilation for development crates. The
host's former `CARGO_INCREMENTAL=0` shell export has been removed; open a new
shell or run `unset CARGO_INCREMENTAL` in existing shells. An explicit environment
override still takes precedence over Cargo profiles.

The generated `chatter-ui-slint` crate remains non-incremental because prior
measurements found its incremental memory use erratic. Dependencies remain
optimized; `chatter-core` stays at O2 to preserve generic attachment-crypto
throughput. Release profiles are unchanged. These settings are not new timing
results; incremental compilation helps subsequent edits, not the first build.

## Opt-in Slint live preview

Run `tools/ui-live-preview.sh` with your usual development features and public
build trust configuration. For example, if the local development anchors and
server are configured:

```sh
tools/ui-live-preview.sh --features mesh,dev-trust-root,dev-auto-register
```

This enables `SLINT_LIVE_PREVIEW=1` and `slint/live-preview` on the command line,
following [Slint's instructions](https://docs.slint.dev/latest/docs/slint/guide/tooling/live-preview/).
It does not add preview to default features. It uses a separate preview data
directory by default and disables OS-keyring access. It does not copy existing
accounts or credentials. Auto-registration additionally requires the existing
`CHATTER_DEV=1` opt-in; use only with the local development server.

The initial build still costs time. Subsequent `.slint` edits can reload in the
running app. Changes to properties/callbacks used by Rust require rebuilding.
Compatibility with this repository's patched Backdrop item and renderer has
not yet been verified in the interpreter. Never package the preview binary.

## Test binaries

Do not blanket-merge the UI integration suites. Several initialize Slint's
process-wide event-loop backend and intentionally have one test per executable.
The [backend documentation](https://docs.slint.dev/latest/docs/rust/i_slint_backend_testing/fn.init_integration_test_with_system_time)
requires one initialization per process. Other suites mutate environment variables
or include application modules with crate-root paths. Consolidation needs an
explicit process-isolating runner and a review of those assumptions first.

## Avoid invalidating or blocking caches

Keep compiler flags, feature sets and target CPU stable between edits. The RC86
baseline CPU flag change caused a full desktop dependency rebuild; Android waited
on the shared Cargo target lock. Build the platforms sequentially when sharing
that directory. Separate target directories avoid the lock but duplicate large
artifacts and do not create more RAM. Do not clean an active build directory.

Mold remains the linker. Wild, Cranelift, workspace feature unification and an
AES/protocol crate split are unmeasured experiments, not applied speedups.
