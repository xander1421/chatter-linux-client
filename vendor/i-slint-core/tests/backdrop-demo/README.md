# Generated layout and rendering regressions

Run from `linux-client` without building the client or its media services:

```sh
cargo test --manifest-path vendor/i-slint-core/tests/backdrop-demo/Cargo.toml --target-dir target --test listview_layout --test opacity_culling --test backdrop_demo
cargo test --manifest-path vendor/i-slint-core/tests/backdrop-demo/Cargo.toml --target-dir target --features skia-opacity --test opacity_skia
```

The optional Skia test needs no display server or account. It uses the actual
patched Skia item renderer with a reused raster target, partial damage and a
small generated scene. It checks hide/reveal, hidden child updates, overflow
outside the opacity group's parent and fractional grouped alpha. The software
backend has a separate reused-buffer test; its existing per-primitive fractional
alpha semantics are checked separately.

Reused buffers matter: an opacity shortcut before the partial-renderer's tracked
backend dispatch can pass a fresh-buffer check while failing to repaint newly
revealed content. Keep the opacity read inside that dispatch. These are pixel and
invalidation checks; the unmodified renderer also passes, so they do not measure
the performance of skipping a transparent subtree. Device traces count the fixed
`Chatter Skia opacity skip transparent` label to establish actual use.

All scene pixels are synthetic. No client files, messages or media are read.
