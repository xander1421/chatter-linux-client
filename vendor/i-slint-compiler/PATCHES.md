# Chatter patches to Slint compiler 1.18.1

Based on the exact public 1.18.1 crate, three-way merged from the patched
1.17.1 fork. Keep upstream licensing files.

## Builtin inventory and 1.18.1 port

Upstream replaced `builtins.slint` with the `builtin_elements.rs` macro DSL.
Every local difference against exact public 1.17.1 was inventoried before the
obsolete file was deleted:

- `Backdrop`: public native item with `blur` and `border-radius` lengths,
  `saturation: 1.0`, `brightness: 0.0`, and `refraction: 0.0` floats. Supports
  children and expands to parent geometry. Both native item and public element
  are registered in `builtin_elements.rs`; no parallel lookup table is needed.
- `StyledTextItem`: `link-clicked(link: string, display-text: string)` reports
  both the destination and visible label, preserving misleading-link checks.
  `pointer-event(PointerEvent)` and read-only `mouse-x` / `mouse-y` expose only
  the link-owned pointer stream so message link holds share the bubble timer;
  non-link gestures and right-click remain bubble-owned. The callback DSL now
  accepts hyphenated argument names to preserve `display-text` exactly.
  Runtime regression source: `crates/chatter-ui/tests/ui_rich_text.rs`.
- `Flickable`: opt-in `immediate-child-press: false` boolean forwards a child's
  press immediately while retaining drag observation and cancellation. The
  corresponding event-routing documentation is retained, along with upstream's
  new mouse-drag policy and content geometry names/viewport aliases.
- `TextInput`: read-only `visual-cursor-x`, `visual-cursor-y`,
  `visual-cursor-width`, and `visual-cursor-height` lengths describe native caret
  geometry in composed text at the IME selection's active endpoint (UTF-8
  bytes), or preedit end without a selection. They remain independent of blink
  and focus visibility and do not mutate selection, preferred-x or IME state.

## Layout and tests

- `layout.rs`: keeps upstream's `has_no_intrinsic_size` helper, including its
  handling of both Builtin and resolved Native classes. Adds Backdrop to that
  helper's transparent-container list instead of restoring the old builtin-only
  match, preserving upstream height-for-width and native-layout improvements.
- `tests/backdrop.rs`: existing all-properties/foreground-child compilation is
  retained. Additional checks cover public native registration, parent-sized
  geometry and numeric defaults; the full patched StyledText/Flickable/caret
  API compiles; output geometry properties remain read-only.
- `Cargo.toml`: retains registration of the focused backdrop test and isolated
  vendor test workspace, alongside new upstream tests/features/dependencies.
  Other compiler source and test additions from upstream remain intact.

Run from linux-client with one job and an isolated target directory:

```sh
CARGO_BUILD_JOBS=1 cargo test --manifest-path vendor/i-slint-compiler/Cargo.toml \
  --test backdrop --test native_class_selection --test single_cell_box_layout \
  --target-dir /tmp/chatter-compiler-target --locked
```

The compiler declaration port must ship with the matching core item/vtable and
Skia renderer changes. Runtime glass pixels/fallback, pointer ownership,
misleading-link safety, Flickable cancellation and reactive caret behavior
remain integration checks; compiler tests establish registration and typing.
