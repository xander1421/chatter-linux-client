# Chatter patches to Slint 1.17.1

Copied from the Cargo registry at exactly 1.17.1. Keep upstream licensing files.

- `builtins.slint`: public `Backdrop` item with blur, saturation (default 1),
  brightness, refraction, and border-radius; children and parent-sized geometry.
  The generic builtin loader registers it; no separate lookup table is needed.
- `layout.rs`: treat Backdrop like the other transparent container items.
- `tests/backdrop.rs`: compile all properties with foreground children.
- `Cargo.toml`: register that test and isolate this vendor's focused test workspace.

Run from linux-client:
`cargo test --manifest-path vendor/i-slint-compiler/Cargo.toml --test backdrop --target-dir target -j 2`

On upgrade, port with the matching core item/vtable and Skia renderer changes,
then run the compiler test, demo software fallback test, and Skia pixel tests.
