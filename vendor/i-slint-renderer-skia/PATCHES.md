# Chatter patches to Slint 1.17.1

Copied from the Cargo registry at exactly 1.17.1 (skia-safe 0.99).
Keep upstream licensing files.

- `backdrop.rs`: blur -> saturation/brightness colour matrix -> optional rounded
  edge refraction RuntimeEffect; cache shader compilation once, plus raster
  pixel and matrix/SkSL tests. Blur is Gaussian sigma in logical pixels, scaled
  once by the renderer. Refraction is clamped to 0..1, brightness to -1..1.
- `itemrenderer.rs`: clip to a rounded rect and initialize SaveLayerRec with the
  backdrop filter, composite it, then paint sharp children. Backdrop reads the
  current compositing layer, so an isolated ancestor layer defines its backdrop.
- `lib.rs`: register the filter module. Core partial-rendering trait advertises
  backdrop support. Android/OpenGL Surface defaults to full repaint; core also
  handles Skia surfaces that opt into partial repaint conservatively.

Effects default ON. Set `SLINT_BACKDROP_EFFECTS=0` (also `off`/`false`) before
process startup to bypass filters; children still render, with rounded clipping.
The app's Appearance.glass-effects setting supplies neutral values at runtime:
blur 0, saturation 1, brightness/refraction 0. Blur 0 alone only disables blur.

Run from linux-client: `cargo test -p i-slint-renderer-skia --lib backdrop::tests -j 2`.
The app demo is `crates/chatter-ui/examples/backdrop_demo.rs`, including a
software pixel fallback test (its file header documents commands).

On upgrade, revalidate SaveLayerRec/backdrop coordinates, filter bounds and edge
sampling, colour-space/alpha behavior, runtime shader compilation, GL rendering,
partial repaint, and Android FPS with effects on/off before shipping.
