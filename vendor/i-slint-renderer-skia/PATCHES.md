# Chatter patches to Slint 1.18.1

Rebased from the exact public Cargo crate 1.18.1 (skia-safe 0.153.3),
using the complete patched 1.17.1 source as the local side of a three-way merge.
Keep upstream licensing files.

- `backdrop.rs`: blur -> saturation/brightness colour matrix -> optional rounded
  edge refraction RuntimeEffect; cache shader compilation once, plus raster
  pixel and matrix/SkSL tests. Blur is Gaussian sigma in logical pixels, scaled
  once by the renderer. Refraction is clamped to 0..1, brightness to -1..1; negative brightness
  scales colour (1 + b) rather than offsetting it, so dark content stays visible.
- `itemrenderer.rs`: initialize SaveLayerRec with the backdrop filter, clear
  its rectangular corner area before compositing, then clip sharp children to
  the rounded rect. The explicit corner mask covers Skia surfaces that apply
  the backdrop before honoring a clip. Backdrop reads the
  current compositing layer, so an isolated ancestor layer defines its backdrop.
- `lib.rs`: register the filter module. Core partial-rendering trait advertises
  backdrop support. Android/OpenGL Surface defaults to full repaint; core also
  handles Skia surfaces that opt into partial repaint conservatively.
- `lib.rs`: release cached layer images with their component, and on Android
  purge Skia's unlocked GPU resource cache after a component closes if at least
  one screen-sized RGBA allocation is purgeable. This releases transient viewer
  surfaces without changing the backdrop filter or its colour matrix.
- `itemrenderer.rs`: release `layer_cache` (not `image_cache`) when a layer's
  `cache-rendering-hint` turns false. If layer creation returns no image, traverse
  children directly, including on subsequent cached-`None` frames, so allocation
  failure never makes content disappear. Successful layer compositing is unchanged.
  Layer target creation also rejects nonpositive sizes and actual rounded target
  areas above 524288 physical pixels, using the same direct fallback. This guards
  descendant overflow beyond the UI's own-geometry eligibility checks; the ceiling
  applies to any rendering hint using this renderer, without changing GPU purges.
- `itemrenderer.rs`: Android-only, allocation-free ATrace sections, gated by
  `ATrace_isEnabled` (API 23+, app minSdk 26): `Chatter layer create` marks an
  actual cache miss/dirty entry's target request, `Chatter layer render`
  marks successful allocation followed by child rendering, and `Chatter layer
  blend` marks every image composite (hits and newly rendered images). For a
  complete capture, blends minus renders counts reused images; creates minus
  renders counts rejected/failed allocations. Cached `None` falls back directly
  with no blend or new create until dependencies change. Names are static; no formatting,
  optional API lookup, counters or per-frame logging are used.

The DM experiment is disabled by default (`MessageList.experimental-content-cache:
false`) pending review of decrypted-texture clearing on lock/background/logout
and device validation. When explicitly enabled, it opts in only the internal
message bubble content layout, with viewport dimensions supplied by the scrolling
list (previews default to zero).
Targets/snapshots are in-process Skia surface/image objects; this cache path does
not encode or write them to disk, and offers no pixel zeroization guarantee.
At this revision, `lib.rs` clears a component's layer cache on destruction, but
`suspend`, `set_surface` and `set_window_adapter` do not explicitly clear it.
Disabling a hint releases its image references when that layer is next traversed;
this is not a lock/background/logout clearing guarantee or a GPU memory wipe.
Only nonempty text without attachments, media quotes, calls, system/deleted/AI
messages, reactions or thread-reply chips qualifies; own messages must have a
stable delivery status. The content must fit both viewport axes and a 384 logical
pixel height ceiling. The product of both rounded-up physical dimensions, using
Slint's window-scale-aware `phx` conversion, is limited to 524288 pixels (~2 MiB
RGBA8 before GPU overhead). This is a per-content texture eligibility bound,
not an aggregate GPU memory limit; realized rows retain eligible images until
invalidation, hint opt-out or component destruction. Glass/backdrop, bubble fill
and rim, progress/highlight overlays, swipe affordances and floating actions are
outside the cached subtree. Absolute-position input/menu bindings stay live.
Build/device validation must check actual allocation bounds, scroll hit rate,
edits/theme/DPI updates, opt-out eviction, allocation failure, interaction anchors
and text/refraction optics before accepting any performance claim.

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

## Android asymmetric rounded-clip path (S9 profiling)

`analytic_clip.rs` avoids device-space software coverage-mask uploads for valid
circular asymmetric rounded rectangles with positive uniform scale/translation
and rectangular parent clips. A thread-local RuntimeEffect retains only the
shader program; bounds, radii and opaque interior regions are uniforms. It
retains no message content, decoded pixels or rendered-content textures.
Unsupported transforms/radii, parent crops through curved corners, shader
failures and nonrectangular parent clips use the existing native Skia path.
Rejection leaves the canvas unchanged. The ordinary rectangular clip supplies
straight-edge coverage. The corner shader divides that coverage out to avoid
double-softening an edge; opaque interior rectangles avoid curve calculations.

The photo-bubble fill in `ui/chat/message_item.slint` relies on its existing outer
rounded clip instead of repeating a rounded fill. A separate child preserves
the curved rim. Glass/backdrop parameters are unchanged. Coverage is not pixel
identical: synthetic S9 GPU comparisons allow differences in the one-device-pixel
antialiasing fringe. The composited fill/rim case measured maximum channel
difference 100/255 and mean 0.408/255 over the 192x192 comparison image, confined
to that fringe. This is a rendering tradeoff, not a pixel-identity claim.

Reproduce host geometry tests and the 36-case on-device, account-independent
GPU checker with `tests/analytic-clip/README.md`. Test fallback canvas state,
fractional shifts, scales, tile clips, viewport crops and fill/rim composition
when upgrading Skia. CPU draw improvements alone do not establish a frame-time
improvement: also measure submission/GPU waits with glass enabled.

- `itemrenderer.rs`: `visit_opacity` returns before traversing children or
  requesting a layer when opacity is nonpositive. The opacity read remains
  inside PartialRenderer's tracked dispatch so reveal invalidates damage. A
  generic `Opacity::render` early return was rejected: it bypassed that tracking
  and failed a reused-buffer reveal test. The retained path is tested against
  actual Skia with a reused raster target and partial damage, including hide,
  hidden child updates, reveal, overflow and fractional grouped opacity. No
  content cache or pixel retention is added. Run from linux-client:
  `cargo test --manifest-path vendor/i-slint-core/tests/backdrop-demo/Cargo.toml --target-dir target --features skia-opacity --test opacity_skia`.

## 1.18.1 migration

All local patch purposes above remain local; none was removed as already upstream.
The shared constructor now initializes `gpu_cleanup_pending` once for all surface
factories. The native Parley visual-caret query and generic WGPU delegation are
retained; the query supplies the input's cursor affinity and the renderer's text
layout cache to the new shared-Parley signature. Other text/font methods use the
new core default implementations through `text_layout_cache`.

The renderer adopts upstream `BoxShadowCache` and its `shape_size`,
`shape_origin`, `outer_radius` and blur helpers, preserving the shadow tracing
scopes. `BorderRectLayout` now computes border geometry, and `combine_clip`
receives the final rectangle/radii with no border-width parameter. The Android
analytic shader still runs only for complex rounded clips, with unchanged
fallback. Upstream transformed rectangle/path antialiasing fixes remain intact.
SVG resolution uses `scalable_render_size`; opaque image detection and explicit
sRGB image metadata are retained alongside the local image trace scopes.

Public backend migration: WGPU 28 support is replaced by WGPU 30; use
`default_wgpu_30`, `SkiaWGPU30Renderer`, `wgpu-30`/`unstable-wgpu-30` features.
WGPU 29 remains available and the generic renderer's new texture APIs are kept.
Metal/Vulkan/Direct3D constructors use upstream surface factory selection.
The standalone analytic-clip harness now pins the same Skia version as this
crate; integration owns regeneration of its test lockfile before `--locked`
validation. Re-run the raster backdrop tests, partial-repaint opacity tests and
36-case device clip checker against the upgraded Skia before shipping.

## Allocation, lifecycle and compact diagnostics (2026-11-04)

Ordinary image fitting uses a single stack entry rather than allocating a Vec. Nine-slice fitting keeps its eager Vec and draw order.288generated geometry/pixel/state cases matched the previous implementation, and an isolated allocation check confirmed zero ordinary-fit allocations. This does not establish an app-wide latency improvement.

The Android backend now calls suspend synchronously on TerminateWindow, before android-activity releases the old window. Existing clear_surface clears image, layer, path, box-shadow and text-layout graphics caches and drops the surface; fonts, core images and app models retain their existing lifetimes. Three real-renderer lifecycle tests cover ordering, idempotence, notifier preservation, failed creation/retry and lost activation. No physical-zeroization or security-bug claim is made.

Exact debug.chatter.compact_trace=1 suppresses detailed renderer scopes and counters while independently retaining EGL observation, native input/render boundaries and selected fixed numeric counters. Each helper caches its own property once. EGL requests one selected identity per eight natural-render attempts using a deterministic mixed block offset; pending records remain bounded to eight and two seconds, polled only on natural renders. Failed requests/swaps may reduce successful samples. Deferred results keep explicit generation/frame identity; no content cache, wait, wake, readback or draw is introduced. Sampled latency must be reported separately from all-frame cadence. Full diagnostics remain the default.

The nondefault experimental-prefix-flush feature is a debug-only investigation, not an accepted performance fix. Balanced S9 tests showed substantially worse media cadence despite text improvement under tracing. Keep the property off in ordinary runs and the feature out of release builds; release gates reject experiment-bearing artifacts.

Glyph-run temporary collection (2026-11-04): up to31glyph IDs and positions
use initialized stack arrays within draw_glyph_run. A31-entry prefix spills at
32, preserving the unknown-length vector growth baseline. Known long runs and
inexact positive lower size hints retain the original mapped unzip path.
Iterator order, casts, offsets, font setup and the single Skia draw are unchanged;
there is no cross-frame scratch or content cache.252synthetic actual-Skia pixel
cases and440capacity/iterator-hint cases pass, including non-fused iteration.
The earlier32-entry candidate was superseded because its unknown long runs
could grow vectors3.125%larger. This is allocation evidence, not an established
end-to-end latency improvement.

The bounded EGL collector also queries optional DEQUEUE_READY and READS_DONE fields (support-mask bits 5 and 6), retaining the first five field positions. It packs all supported names into one query per retained record, with independent pending/invalid retirement and explicit generation/frame identity per result. Neither endpoint proves GPU busy time or the cause of a blocked call; runtime support must be measured. Nine collector policy tests, all 127 nonempty support subsets, delayed identity, closed-label and Android metadata checks pass. Full Android/device verification is recorded in the S9 audit report.
