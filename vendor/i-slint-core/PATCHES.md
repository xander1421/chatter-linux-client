# Chatter patches to Slint 1.17.1

Copied from the Cargo registry at exactly 1.17.1. Keep upstream licensing files.

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
`crates/chatter-ui/tests/ui_rich_text.rs`; no vendor version upgrade.
