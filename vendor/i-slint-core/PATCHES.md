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
