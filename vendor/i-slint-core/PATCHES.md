# Chatter patches to Slint 1.18.1

Rebased by three-way comparison of the public 1.17.1 and 1.18.1 Cargo sources. Keep upstream licensing files. Integration validation is recorded separately.

ListView layout reuse (2026-11-03): `model/repeater.rs` now uses its previously
unused geometry tracker. Unchanged calls retain redraw dependencies while
skipping virtualize/layout work; viewport, row geometry, model row-count and
model replacement changes invalidate it. Width and height are compared numerically. The typed path reuses geometry;
arbitrary callback-backed properties force a refresh, including the first typed
call after a callback, to avoid stale property identity. Child instantiation still runs on every call.
The tracker retains property dependencies and a numeric width, no row data or
rendered pixels. `layout_tracker_tests` cover invalidation and dependency reuse;
the standalone harness exercises generated ListView scrolling, resize, row edits,
offscreen append, emptying and re-population:
`cargo test --manifest-path vendor/i-slint-core/tests/backdrop-demo/Cargo.toml --target-dir target --test listview_layout`.

- `items.rs`: Backdrop item, properties, vtable, geometry and event passthrough.
- `item_rendering.rs`: `visit_backdrop` defaults to rendering children unchanged;
  supporting renderers declare `SUPPORTS_BACKDROP`. Visit Backdrop even outside
  the damage clip so a supporting renderer can clip its children correctly.
- `partial_renderer.rs`: forward Backdrop visits and conservatively repaint the
  component when active backdrop optics are present, including unchanged glass
  above changed content. Neutral effects retain ordinary partial repaint behavior.
- `styled_text.rs`: `StyledText::from_paragraphs` plus re-exports of
  `StyledTextParagraph` / `FormattedSpan` / `Style`, so chat message bodies are
  built from the client's own markdown spans (`chatter-ui`
  `converters/markdown.rs`). Upstream's only constructors are plain text and
  `from_markdown`, which rejects headings, quotes, fenced code and HTML.
- `items/text.rs` `StyledTextItem::input_event`: ignore pointer events unless
  they are over a link (and all of them when the text has no links). Upstream
  accepted every press, move and release, so a `TouchArea` behind or around a
  `StyledText` never saw a press: message long-press, right-click and tap died
  on styled text. Covered by `crates/chatter-ui/tests/ui_rich_text.rs`.

The software, femtovg and testing renderers need no patches: their default
visitor keeps children visible. No core-macros changes are needed.

On upgrade, retain the default visitor and trait default for compatibility, and
recheck damage tracking and all renderers with the matching compiler/Skia patch.

Standalone demo/fallback check without application media dependencies:
`cargo test --manifest-path vendor/i-slint-core/tests/backdrop-demo/Cargo.toml --target-dir target -j 2`.
It compiles the actual app demo against the patched core/compiler and asserts
software-rendered foreground/background pixels with optics enabled.


Chatter message web links (2026-10-06): `items/text.rs` `StyledTextItem`
records the left-press URL, position, finger and platform clock. Only a
matching release under 500 ms and within 8 logical pixels activates the link.
Drag, exit, wheel, unmatched release and changed URL/finger revoke activation;
right-button events pass through to the bubble/context menu. Non-link text
still ignores events as before. Link presses grab their stream and emit
`pointer-event` with `mouse-x`/`mouse-y`, registered by the matching compiler
builtin patch. `MessageItem` routes that stream and ordinary bubble presses
through shared hold functions: mobile link holds open the message menu, while
release never launches a URL after a hold; desktop left holds remain inert.
Drag/cancel revoke the pending URL and stop the message timer. Right-click and
non-link presses remain bubble-owned. Selection-mode taps toggle once through
the link callback, guarded against an already-fired hold. Coverage is added to
`crates/chatter-ui/tests/ui_rich_text.rs`; retained across the 1.18.1 upgrade.

Link drag correction (2026-10-06): grabbed movement now also cancels when
the pointer leaves the item bounds or the pressed URL, even inside the 8 px
movement slop. Cancellation takes the pending press before notifying the
message's existing Cancel callback, so returning before release cannot revive
the URL or its hold timer. The rich-text movement/exit regressions wait for
Flickable's 100 ms delayed Down delivery before exercising the link grab;
the drag and item-exit cases explicitly move back before release.

Android composition (C4, 2026-10-11): `textlayout/sharedparley.rs` paints only
`selection_range`, not the entire `preedit_range`. The native Android callback
now supplies the actual anchor/caret relative to preedit; a composing word with
a collapsed caret keeps ordinary text colors. GameActivity already delivers
composition as committed text while preserving its native composing region.

Single-line bidi elision (2026-10-26): `textlayout/sharedparley.rs` cuts plain,
non-wrapping elided text at a grapheme boundary in logical order, appends U+2026
inside any open isolate, then shapes the result. If the cut is immediately
before the body's FSI, it keeps that opener and appends PDI after the ellipsis.
Parley resolves the base direction from the first strong character outside
isolates (UAX #9 P2/P3). The regression test now follows `draw_text` through
`create_text_paragraphs` and `layout()` with the row's no-wrap, left-aligned
elided settings, and checks the leftmost visible cluster. The existing visual
glyph elision still handles styled, wrapped and multiline text.

Hidden focus item (2026-10-29): `window.rs` `process_key_input` drops focus when
the focused item is no longer visible, and finds nothing when it was removed (a
dialog tab or settings page switched away). Upstream then delivers the key to
nothing, so Escape did nothing until a click (desktop row menu after a wheel
scroll; New channel after switching to Join; Settings after leaving a focused
field's page). `publish_focus_item` now records the focused item's ancestors,
and such a key is delivered from the nearest one still alive and visible, so
window-level handlers (the app's back scope) still receive it.

Clip rect cost (2026-10-29): `item_tree.rs` `absolute_clip_rect_and_geometry`
called itself for each clipping ancestor and discarded that ancestor's clip,
so the work doubled with every nested clip (2^n). The 1.18.1 upstream transform-based ancestor geometry supersedes the local fix;
no recursive clip computation is retained. Every `is_visible()` pays
this, including each node of an Android accessibility snapshot.

- `window.rs` `move_focus` modal Tab trap (2026-10-29): Tab and Shift+Tab walked
  out of every open dialog and menu into the sidebar and composer behind it
  (headless desktop, Ctrl+/ dialog and the chat ⋯ menu). While a visible
  `accessible-id: "modal-overlay"` item is open (the frontmost one, same rule as
  the winit AccessKit adapter), Tab navigation only accepts items inside it.

## Traversal allocation and concrete dispatch (2026-11-04)

Repeater child traversal takes an ephemeral ownership snapshot before callbacks: up to four slots use inline storage; larger snapshots reserve one Vec. Holes, order, complete traversal, dependency registration and callback-driven model replacement remain intact. No child or message cache is added. Generated init-driven reset/refill cases cover both storage paths. Non-FFI repeated child ensuring calls the concrete component method; FFI retains the erased helper for its original unwind boundary. Full core suite345tests passed; device latency improvement is not established.

Android compact diagnostics are opt-in via exact debug.chatter.compact_trace=1, cached once by this helper. They suppress core diagnostic scopes/counters only, without gating model/layout/traversal work. The three participating crates cache independently; changing the property requires restarting the process.
